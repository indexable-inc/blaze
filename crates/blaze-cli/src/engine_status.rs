//! A stopped or asked `blaze engine-check` says where it stands (rules 15, 16, 18): SIGTERM or
//! SIGINT stops it, its status written to the profile and to stderr and the process ended with
//! a typed status within [`STOP_BOUND`]; SIGINFO (macOS's Ctrl-T) or SIGUSR1 writes the status
//! and the check goes on; `--status-every <seconds>` writes one compact line each interval.
//!
//! The parts:
//!
//! - [`Board`]: what the workers and the driver say as they go, in atomics and small locks
//!   only: each stage's tasks known, started and done, its wall and its tasks' summed time, its
//!   memory at its end (engine_memory.rs `AtEnd`), and each running task in a slot (label,
//!   start, its frame's bytes when it began). A task starts through [`Board::start`], the
//!   workers' scheduling point: once a stop is asked it starts no more.
//! - [`write_status`]: the one printer of the status, the stop and the end profile's rows.
//! - [`Watch`]: the watcher's sans-IO core (rule 14), a function of the time and the requests.
//! - [`watched`]: its thin shell, a scoped thread that wakes every [`TICK`] (rule 18), runs
//!   only when a signal can ask or a line is due. When the workers reach no scheduling point
//!   in [`STOP_BOUND`], it writes the stop from the counters, names each task that did not
//!   stop, and ends the process with `Exit::StopStalled`.
//!
//! The signals' handlers only store into [`Requests`] (blaze-core run_signals.rs `Asked`),
//! handed to the driver by its edge (rule 75); no core reads a global.

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use core::time::Duration;

use blaze_core::lock::{Lock, Signal};
use blaze_engine::{STAGES, Stage};
use blaze_own::FixedString;
use bstd::clock::Clock;
use bstd::fs::{Dir, ReadWrite};
use bstd::host::Host;
use bstd::out::Out;
use bstd::path::Path;
use bstd::procs::{By, Requests};
use bstd::Cx;

use crate::engine_memory::AtEnd;

/// Running tasks the board names at once: past it a task is counted, never named.
pub(crate) const SLOTS: usize = 256;
/// The bytes of a task's label kept (cut at a character).
const LABEL: usize = 96;
/// How many running tasks the status names, the longest first.
pub(crate) const LONGEST: usize = 10;
/// From a stop asked to the process ended, at most (the brief's 2 s).
pub(crate) const STOP_BOUND: Duration = Duration::from_secs(2);
/// The watcher's wake: a tenth of [`STOP_BOUND`].
const TICK: Duration = Duration::from_millis(100);
/// The status text's most bytes, on the watcher's stack.
const TEXT: usize = 32 << 10;
/// The watcher's context's scratch.
const WATCH_SCRATCH: usize = 1 << 20;

/// A task's label as the status prints it: inline, so a slot allocates nothing.
#[derive(Clone, Copy)]
pub(crate) struct Label {
    bytes: [u8; LABEL],
    len: u8,
}

impl Label {
    pub(crate) const EMPTY: Label = Label { bytes: [0; LABEL], len: 0 };

    /// `text`, cut at the last character that fits.
    pub(crate) fn of(text: &str) -> Label {
        let mut label = Label::EMPTY;
        let _ = label.write_str(text);
        label
    }

    /// `value` as `{:?}` prints it, cut as [`Label::of`].
    pub(crate) fn debug(value: &impl fmt::Debug) -> Label {
        let mut label = Label::EMPTY;
        let _ = write!(label, "{value:?}");
        label
    }

    pub(crate) fn as_str(&self) -> &str {
        // Whole characters alone are written (`write_str`): the bytes are UTF-8.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

impl fmt::Write for Label {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let at = usize::from(self.len);
        let room = LABEL - at;
        let mut take = text.len().min(room);
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[at..at + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len = (at + take) as u8;
        if take < text.len() { Err(fmt::Error) } else { Ok(()) }
    }
}

/// A test's plant (rule 23): a request made by the board itself when `stage` has done `done`
/// tasks, deterministic on one worker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    StopAfter { stage: Stage, done: u64, by: By },
    StatusAfter { stage: Stage, done: u64, by: By },
}

/// What may ask the command, given by its edge: the process's signals, a line's interval, a
/// test's plant.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Asking<'r> {
    pub(crate) requests: Option<&'r Requests>,
    pub(crate) every: Option<Duration>,
    pub(crate) plant: Plant,
}

impl Asking<'_> {
    /// Nothing asks: a test's run, or `why`.
    pub(crate) const NONE: Asking<'static> = Asking { requests: None, every: None, plant: Plant::None };
}

/// One stage's counts.
#[derive(Default)]
struct Counts {
    known: AtomicU64,
    started: AtomicU64,
    done: AtomicU64,
    /// Starts refused by a stop.
    refused: AtomicU64,
    /// Its tasks' time summed, and its phases' wall summed (ns).
    cpu_ns: AtomicU64,
    wall_ns: AtomicU64,
    /// When its running phase began, plus 1 (0: none runs).
    phase_from: AtomicU64,
}

/// A running task or live sub-activity.
#[derive(Clone, Copy)]
struct Slot {
    stage: Option<Stage>,
    kind: blaze_screen::WorkKind,
    label: Label,
    from_ns: u64,
    /// Its frame's bytes used when it began (a worker's frame is not read from another thread).
    frame: u64,
}

/// Default [`blaze_screen::WorkKind`] for an engine [`Stage`].
const fn default_work_kind(stage: Stage) -> blaze_screen::WorkKind {
    match stage {
        Stage::Scripts => blaze_screen::WorkKind::BuildScript,
        Stage::Units | Stage::Outlines | Stage::Names | Stage::Heads | Stage::Bodies | Stage::Lints => {
            blaze_screen::WorkKind::Blaze
        }
    }
}

/// Concise lowercase name for a Blaze checking stage in the live TUI hint.
const fn stage_hint_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Units => "units",
        Stage::Scripts => "scripts",
        Stage::Outlines => "outlines",
        Stage::Names => "names",
        Stage::Heads => "heads",
        Stage::Bodies => "bodies",
        Stage::Lints => "lints",
    }
}

/// Snapshot of active tasks grouped by [`blaze_screen::WorkKind`], slowest-first per group.
struct Grouped {
    top: [[Option<Slot>; blaze_screen::MAX_GROUP_ITEMS]; blaze_screen::WORK_KINDS],
    counts: [u32; blaze_screen::WORK_KINDS],
    active: u32,
    blaze_stages: u8,
}

/// Who writes the stopped status: the first to claim it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Claim {
    Driver = 1,
    Watcher = 2,
}

/// What the workers and the driver say as they go (the module's doc). Times are ns since the
/// command's start, as [`Board::now`] reads them.
pub(crate) struct Board<'r> {
    asked: Option<&'r Requests>,
    /// The plant's requests.
    own: Requests,
    plant: Plant,
    began: Duration,
    counts: [Counts; STAGES],
    at_end: Lock<[AtEnd; STAGES]>,
    /// Each stage's phases' arenas and the heap kept past them, as of its last phase's end
    /// (engine_phase_mem.rs).
    phase_mem: Lock<[crate::engine_phase_mem::StageMem; STAGES]>,
    /// What each checked body left past its task and what it took in its scratch, summed and
    /// at its most (engine_bodies.rs `BodyMemLive`, lane BD3): the `bodies memory:` line.
    pub(crate) bodies_mem: crate::engine_bodies::BodyMemLive,
    slots: [Lock<Option<Slot>>; SLOTS],
    cursor: AtomicUsize,
    unslotted: AtomicU64,
    /// The stage whose phase began last, plus 1 (0: none yet).
    stage: AtomicU8,
    claimed: AtomicU8,
    /// The work is over: the watcher ends.
    over: AtomicBool,
}

/// A task started on the board: [`Running::end`] when it ends; dropped without it (a panic),
/// it only frees its slot.
pub(crate) struct Running<'b> {
    board: &'b Board<'b>,
    stage: Stage,
    slot: Option<usize>,
    from_ns: u64,
}

/// A live sub-activity on the board (such as a Wasmtime proc-macro expansion running inside a
/// `Names` or `Bodies` task): frees its slot on drop without altering stage task counts.
pub(crate) struct ActivityGuard<'b> {
    board: &'b Board<'b>,
    slot: Option<usize>,
}

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.and_then(|at| self.board.slots.get(at)) {
            *slot.lock() = None;
        }
    }
}

/// A phase begun: what [`Board::phase_ended`] tops up from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PhaseMark {
    stage: Stage,
    started: u64,
    done: u64,
    refused: u64,
}

impl<'r> Board<'r> {
    /// A board for a command that began at `began` on its clock.
    pub(crate) fn new(asking: &Asking<'r>, began: Duration) -> Board<'r> {
        Board {
            asked: asking.requests,
            own: Requests::new(),
            plant: asking.plant,
            began,
            counts: core::array::from_fn(|_| Counts::default()),
            at_end: Lock::new("engine.status.at_end", [AtEnd::default(); STAGES]),
            phase_mem: Lock::new("engine.status.phase_mem", [crate::engine_phase_mem::StageMem::default(); STAGES]),
            bodies_mem: crate::engine_bodies::BodyMemLive::new(),
            slots: [const { Lock::new("engine.status.slot", None) }; SLOTS],
            cursor: AtomicUsize::new(0),
            unslotted: AtomicU64::new(0),
            stage: AtomicU8::new(0),
            claimed: AtomicU8::new(0),
            over: AtomicBool::new(false),
        }
    }

    /// Now on `clock`, in ns since the command began.
    pub(crate) fn now(&self, clock: &Clock) -> u64 {
        ns(clock.now().since_start().saturating_sub(self.began))
    }

    /// The stop asked, if one was.
    pub(crate) fn stopping(&self) -> Option<By> {
        self.asked.and_then(Requests::stop).or_else(|| self.own.stop())
    }

    /// The status asked since the last take, taken.
    pub(crate) fn take_status(&self) -> Option<By> {
        self.asked.and_then(Requests::take_status).or_else(|| self.own.take_status())
    }

    /// The stage whose phase began last.
    pub(crate) fn stage(&self) -> Option<Stage> {
        Stage::ALL.get(usize::from(self.stage.load(Ordering::Acquire)).checked_sub(1)?).copied()
    }

    /// `who` writes the stopped status, unless the other claimed it first.
    pub(crate) fn claim(&self, who: Claim) -> bool {
        self.claimed.compare_exchange(0, who as u8, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// A phase of `stage` begins now with `known` tasks.
    pub(crate) fn phase_began(&self, stage: Stage, known: u64, now: u64) -> PhaseMark {
        let counts = &self.counts[stage.index()];
        counts.known.fetch_add(known, Ordering::AcqRel);
        counts.phase_from.store(now.saturating_add(1), Ordering::Release);
        self.stage.store(stage.index() as u8 + 1, Ordering::Release);
        PhaseMark { stage, started: counts.started.load(Ordering::Acquire), done: counts.done.load(Ordering::Acquire), refused: counts.refused.load(Ordering::Acquire) }
    }

    /// The phase of `mark` ended now, having entered `entered` tasks: its wall added, and the
    /// tasks no scheduling point counted (a stage that starts none on the board) counted done,
    /// unless a stop refused one of its starts (the rest were then not run).
    pub(crate) fn phase_ended(&self, mark: PhaseMark, entered: u64, now: u64) {
        let counts = &self.counts[mark.stage.index()];
        let from = counts.phase_from.swap(0, Ordering::AcqRel);
        counts.wall_ns.fetch_add(now.saturating_sub(from.saturating_sub(1)), Ordering::AcqRel);
        if counts.refused.load(Ordering::Acquire) != mark.refused {
            return;
        }
        let started = counts.started.load(Ordering::Acquire) - mark.started;
        let done = counts.done.load(Ordering::Acquire) - mark.done;
        counts.started.fetch_add(entered.saturating_sub(started), Ordering::AcqRel);
        counts.done.fetch_add(entered.saturating_sub(done), Ordering::AcqRel);
    }

    /// A stage's memory at its end (engine_memory.rs `Memory::merged`'s row), for the status.
    pub(crate) fn merged(&self, stage: Stage, at: AtEnd) {
        self.at_end.lock()[stage.index()] = at;
    }

    /// A stage's phases' memory after one of its phases ended (engine_phase_mem.rs
    /// `PhaseLedger::add`), for the status.
    pub(crate) fn phase_mem(&self, stage: Stage, at: crate::engine_phase_mem::StageMem) {
        self.phase_mem.lock()[stage.index()] = at;
    }

    /// A task of `stage` starts now, its frame at `frame` bytes: the workers' scheduling
    /// point. `None` once a stop is asked: the task is not run.
    pub(crate) fn start(&self, stage: Stage, label: impl FnOnce() -> Label, frame: u64, now: u64) -> Option<Running<'_>> {
        if self.stopping().is_some() {
            self.refused(stage, 1);
            return None;
        }
        Some(self.show(stage, label(), frame, now))
    }

    /// `tasks` of `stage` not begun, refused by a stop.
    pub(crate) fn refused(&self, stage: Stage, tasks: u64) {
        self.counts[stage.index()].refused.fetch_add(tasks, Ordering::AcqRel);
    }

    /// A task of `stage` starts now and runs whatever is asked (a stage whose tasks are begun
    /// all at once: its stop is the driver's, after them).
    pub(crate) fn show(&self, stage: Stage, label: Label, frame: u64, now: u64) -> Running<'_> {
        self.show_kind(stage, default_work_kind(stage), label, frame, now)
    }

    /// A task of `stage` with explicit [`blaze_screen::WorkKind`] attribution starts now.
    pub(crate) fn show_kind(
        &self,
        stage: Stage,
        kind: blaze_screen::WorkKind,
        label: Label,
        frame: u64,
        now: u64,
    ) -> Running<'_> {
        self.counts[stage.index()].started.fetch_add(1, Ordering::AcqRel);
        let slot = self.claim_slot(Slot { stage: Some(stage), kind, label, from_ns: now, frame });
        if slot.is_none() {
            self.unslotted.fetch_add(1, Ordering::AcqRel);
        }
        Running { board: self, stage, slot, from_ns: now }
    }

    /// A live sub-activity (e.g. Wasmtime proc-macro expansion) starts now: shown in the live TUI
    /// and status dumps without modifying stage task counters.
    pub(crate) fn show_activity(
        &self,
        kind: blaze_screen::WorkKind,
        label: Label,
        now: u64,
    ) -> ActivityGuard<'_> {
        let slot = self.claim_slot(Slot { stage: None, kind, label, from_ns: now, frame: 0 });
        ActivityGuard { board: self, slot }
    }

    fn claim_slot(&self, entry: Slot) -> Option<usize> {
        let first = self.cursor.fetch_add(1, Ordering::Relaxed);
        for step in 0..SLOTS {
            let at = (first + step) % SLOTS;
            let Some(slot) = self.slots.get(at) else { continue };
            let mut held = slot.lock();
            if held.is_none() {
                *held = Some(entry);
                return Some(at);
            }
        }
        None
    }

    /// Active tasks grouped by [`blaze_screen::WorkKind`], slowest-first (up to
    /// [`blaze_screen::MAX_GROUP_ITEMS`] per category), plus active Blaze stage bitmask.
    fn grouped(&self) -> Grouped {
        let mut top: [[Option<Slot>; blaze_screen::MAX_GROUP_ITEMS]; blaze_screen::WORK_KINDS] =
            [[None; blaze_screen::MAX_GROUP_ITEMS]; blaze_screen::WORK_KINDS];
        let mut counts = [0u32; blaze_screen::WORK_KINDS];
        let mut active = u32::try_from(self.unslotted.load(Ordering::Acquire)).unwrap_or(u32::MAX);
        let mut blaze_stages = 0u8;
        for slot in self.slots.iter() {
            let Some(one) = *slot.lock() else { continue };
            active = active.saturating_add(1);
            let k = one.kind.index();
            if let Some(count) = counts.get_mut(k) {
                *count = count.saturating_add(1);
            }
            if one.kind == blaze_screen::WorkKind::Blaze
                && let Some(stage) = one.stage
            {
                let bit = u8::try_from(stage.index()).unwrap_or(7);
                if bit < 8 {
                    blaze_stages |= 1u8 << bit;
                }
            }
            if let Some(row) = top.get_mut(k)
                && let Some(at) = row.iter().position(|existing| existing.is_none_or(|e| one.from_ns < e.from_ns))
            {
                row.copy_within(at..blaze_screen::MAX_GROUP_ITEMS - 1, at + 1);
                if let Some(cell) = row.get_mut(at) {
                    *cell = Some(one);
                }
            }
        }
        Grouped { top, counts, active, blaze_stages }
    }

    /// Each running task, the longest first (ties by slot), at most [`LONGEST`]; and how many
    /// run, named or not.
    fn longest(&self) -> ([Option<Slot>; LONGEST], u64) {
        let mut kept: [Option<Slot>; LONGEST] = [None; LONGEST];
        let mut running = self.unslotted.load(Ordering::Acquire);
        for slot in self.slots.iter() {
            let Some(one) = *slot.lock() else { continue };
            running += 1;
            let Some(at) = kept.iter().position(|k| k.is_none_or(|k| one.from_ns < k.from_ns)) else { continue };
            kept.copy_within(at..LONGEST - 1, at + 1);
            kept[at] = Some(one);
        }
        (kept, running)
    }

    /// The work is over: the watcher ends at its next wake.
    fn finish(&self) {
        self.over.store(true, Ordering::Release);
    }
}

impl Running<'_> {
    /// The task ended now.
    pub(crate) fn end(self, now: u64) {
        let board = self.board;
        let counts = &board.counts[self.stage.index()];
        counts.cpu_ns.fetch_add(now.saturating_sub(self.from_ns), Ordering::AcqRel);
        let done = counts.done.fetch_add(1, Ordering::AcqRel) + 1;
        match board.plant {
            Plant::StopAfter { stage, done: at, by } if stage == self.stage && done == at => board.own.ask_stop(by),
            Plant::StatusAfter { stage, done: at, by } if stage == self.stage && done == at => board.own.ask_status(by),
            _ => {}
        }
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if let Some(at) = self.slot {
            *self.board.slots[at].lock() = None;
        }
    }
}

/// What the status text says first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Heading {
    /// Asked by a signal, the check going on.
    Asked { by: By },
    /// Stopped by a signal; `stuck`: written by the watcher, the workers not stopped in time.
    Stopped { by: By, stuck: bool },
    /// The end profile's rows.
    End,
}

/// The status, the stop's text or the end profile's rows, at `now`: the one printer.
pub(crate) fn write_status(board: &Board<'_>, heading: Heading, now: u64, into: &mut impl fmt::Write) -> fmt::Result {
    let at = Secs(now);
    match heading {
        Heading::Asked { by } => writeln!(into, "status at {at} s (asked by {})", by.name())?,
        Heading::Stopped { by, .. } => match board.stage() {
            Some(stage) => writeln!(into, "stopped at {at} s by {} in stage {stage:?}", by.name())?,
            None => writeln!(into, "stopped at {at} s by {} before the first stage", by.name())?,
        },
        Heading::End => writeln!(into, "stages at the end, {at} s:")?,
    }
    let running = board.stage();
    for stage in Stage::ALL {
        let c = &board.counts[stage.index()];
        let from = c.phase_from.load(Ordering::Acquire);
        let wall = c.wall_ns.load(Ordering::Acquire) + if from > 0 { now.saturating_sub(from - 1) } else { 0 };
        let mark = if from > 0 && running == Some(stage) { " (running)" } else { "" };
        writeln!(
            into,
            "stage {stage:?}{mark}: tasks {} done / {} started / {} known · wall {} s · cpu {} s",
            c.done.load(Ordering::Acquire),
            c.started.load(Ordering::Acquire),
            c.known.load(Ordering::Acquire),
            Secs(wall),
            Secs(c.cpu_ns.load(Ordering::Acquire))
        )?;
    }
    let at_end = *board.at_end.lock();
    for stage in Stage::ALL {
        if at_end[stage.index()].seen {
            crate::engine_memory::at_end_row(stage, &at_end[stage.index()], into)?;
            writeln!(into)?;
        }
    }
    // A stage's phases' arenas and what the heap kept past them: one line a stage that ran a
    // phase (engine_phase_mem.rs `row`).
    let phase_mem = *board.phase_mem.lock();
    for stage in Stage::ALL {
        if phase_mem[stage.index()].phases > 0 {
            crate::engine_phase_mem::row(stage, &phase_mem[stage.index()], into)?;
            writeln!(into)?;
        }
    }
    // What a checked body leaves past its task, by part, and its scratch (engine_bodies.rs
    // `BodyMemLive`): nothing when no body was checked.
    board.bodies_mem.line(into)?;
    let s = blaze_mem::tag::snapshot();
    write!(into, "memory now (bytes): regions {} frames {} heap {}", s.resident_committed(), s.get(blaze_mem::Tag::Frames).live, s.heap_outside.saturating_add(s.heap()))?;
    match blaze_core::own_footprint() {
        Some((now_bytes, most)) => writeln!(into, " · footprint {now_bytes} (most {most})")?,
        None => writeln!(into, " · footprint unknown (the system did not answer)")?,
    }
    // Each tag's share (blaze-mem tag.rs `Counters`), which the totals above do not name
    // (tempo, 2026-11-18, tree 2c98ace5cf: the Bodies stage took 11 GB of regions in 21 s and
    // the line said only "regions"; rule 16). Every tag holding 64 MiB or more, largest first.
    let mut tags: [(blaze_mem::Tag, u64); blaze_mem::Tag::COUNT] = blaze_mem::Tag::ALL.map(|tag| (tag, s.get(tag).live));
    tags.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    write!(into, "memory by tag (bytes):")?;
    for (tag, live) in tags.iter().filter(|(_, live)| *live >= 64 << 20) {
        let c = s.get(*tag);
        write!(into, " · {tag:?} live {live} (most {}, reserved {}, regions {}, heap {})", c.high_water, c.reserved, c.regions, c.heap)?;
    }
    writeln!(into)?;
    let (longest, count) = board.longest();
    write!(into, "running: {count} task(s)")?;
    for one in longest.iter().flatten() {
        match one.stage {
            Some(stage) => write!(into, " · `{}` ({stage:?}) {} s, frame {} bytes", one.label.as_str(), Secs(now.saturating_sub(one.from_ns)), one.frame)?,
            None => write!(into, " · `{}` ({}) {} s", one.label.as_str(), one.kind.label("blaze"), Secs(now.saturating_sub(one.from_ns)))?,
        }
    }
    writeln!(into)?;
    if let Heading::Stopped { stuck: true, .. } = heading {
        for one in longest.iter().flatten() {
            writeln!(into, "did not stop: `{}` (running {} s)", one.label.as_str(), Secs(now.saturating_sub(one.from_ns)))?;
        }
        let unnamed = count.saturating_sub(longest.iter().flatten().count() as u64);
        if unnamed > 0 {
            writeln!(into, "did not stop: {unnamed} more task(s) not named (past the {LONGEST} longest)")?;
        }
    }
    Ok(())
}

/// The periodic line: `t 120 s · Bodies 4180/9120 tasks · footprint 9.8 GB (most 10.1) · frames
/// 6.2 GB · heap 0.9 GB · longest: tempo_node::run 41 s`.
pub(crate) fn line(board: &Board<'_>, now: u64, into: &mut impl fmt::Write) -> fmt::Result {
    write!(into, "t {} s", now / 1_000_000_000)?;
    if let Some(stage) = board.stage() {
        let c = &board.counts[stage.index()];
        write!(into, " · {stage:?} {}/{} tasks", c.done.load(Ordering::Acquire), c.known.load(Ordering::Acquire))?;
    }
    match blaze_core::own_footprint() {
        Some((now_bytes, most)) => write!(into, " · footprint {} GB (most {})", Gb(now_bytes), Gb(most))?,
        None => write!(into, " · footprint unknown")?,
    }
    let s = blaze_mem::tag::snapshot();
    write!(into, " · frames {} GB · heap {} GB", Gb(s.get(blaze_mem::Tag::Frames).live), Gb(s.heap_outside.saturating_add(s.heap())))?;
    // The stage whose ended phases kept the most on the heap (engine_phase_mem.rs): the debt
    // named as it grows; the whole row a stage is the status's.
    let phase_mem = *board.phase_mem.lock();
    let most = Stage::ALL.into_iter().filter(|stage| phase_mem[stage.index()].phases > 0).max_by_key(|stage| phase_mem[stage.index()].kept_on_heap);
    if let Some(stage) = most {
        let kept = u64::try_from(phase_mem[stage.index()].kept_on_heap.max(0)).unwrap_or(0);
        write!(into, " · kept past its phases: {stage:?} {} GB", Gb(kept))?;
    }
    let grouped = board.grouped();
    // Explicitly attribute slow external work (`build.rs`, `fetch`, `proc-macro`) on periodic
    // plain log lines so scripted / piped runs separate external script time from Blaze time.
    for kind in [blaze_screen::WorkKind::BuildScript, blaze_screen::WorkKind::Fetch, blaze_screen::WorkKind::ProcMacro] {
        let k = kind.index();
        let total = grouped.counts.get(k).copied().unwrap_or(0);
        if total == 0 {
            continue;
        }
        match kind {
            blaze_screen::WorkKind::BuildScript => write!(into, " · build.rs [external]: ")?,
            blaze_screen::WorkKind::Fetch => write!(into, " · fetch: ")?,
            blaze_screen::WorkKind::ProcMacro => write!(into, " · proc-macro: ")?,
            blaze_screen::WorkKind::Blaze => {}
        }
        let mut shown = 0u32;
        if let Some(row) = grouped.top.get(k) {
            for one in row.iter().flatten() {
                if shown > 0 {
                    write!(into, ", ")?;
                }
                write!(into, "{} {} s", one.label.as_str(), now.saturating_sub(one.from_ns) / 1_000_000_000)?;
                shown += 1;
            }
        }
        let extra = total.saturating_sub(shown);
        if extra > 0 {
            write!(into, " +{extra}")?;
        }
    }
    let (longest, _) = board.longest();
    if let Some(one) = longest[0] {
        write!(into, " · longest: {} {} s", one.label.as_str(), now.saturating_sub(one.from_ns) / 1_000_000_000)?;
    }
    writeln!(into)
}

/// What the watcher does at a look.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Act {
    Nothing,
    Status(By),
    Line,
    /// The workers did not stop within [`STOP_BOUND`]: the stop is the watcher's.
    Stopped(By),
}

/// The watcher's core (rule 14): a function of the time and what is asked.
#[derive(Debug)]
pub(crate) struct Watch {
    every: Option<u64>,
    next_line: u64,
    stop_from: Option<(By, u64)>,
    done: bool,
}

impl Watch {
    pub(crate) fn new(every: Option<Duration>) -> Watch {
        let every = every.map(ns).filter(|every| *every > 0);
        Watch { every, next_line: every.unwrap_or(0), stop_from: None, done: false }
    }

    /// At `now`: the stop asked, the status asked (taken), whether the driver claimed the stop.
    pub(crate) fn look(&mut self, now: u64, stop: Option<By>, status: Option<By>, claimed: bool) -> Act {
        if self.done {
            return Act::Nothing;
        }
        if let Some(by) = stop {
            let (by, from) = *self.stop_from.get_or_insert((by, now));
            if claimed {
                self.done = true;
            } else if now.saturating_sub(from) >= ns(STOP_BOUND) {
                self.done = true;
                return Act::Stopped(by);
            }
            return Act::Nothing;
        }
        if let Some(by) = status {
            return Act::Status(by);
        }
        match self.every {
            Some(every) if now >= self.next_line => {
                self.next_line = now.saturating_add(every);
                Act::Line
            }
            _ => Act::Nothing,
        }
    }
}

/// Why the watcher could not watch.
#[derive(Debug)]
pub(crate) enum StatusError {
    /// Its thread was not started.
    Watcher { kind: std::io::ErrorKind },
    /// Its context was not made.
    Context(bstd::Stopped),
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatusError::Watcher { kind } => write!(f, "the status watcher's thread did not start: {kind:?} (engine_status.rs `watched`)"),
            StatusError::Context(why) => write!(f, "the status watcher's context: {why:?} (engine_status.rs `watched`)"),
        }
    }
}

impl core::error::Error for StatusError {}

/// `text` to stderr and to the profile, replaced whole by a rename (bstd `Dir::publish`).
pub(crate) fn put(cx: &Cx, root: &Dir<ReadWrite>, err: &Out, text: &[u8]) -> Result<(), crate::CliError> {
    crate::print::ensure_checkout_ignore(cx, root)?;
    err.write_all(cx, text)?;
    err.flush(cx)?;
    root.publish(cx, &Path::new(crate::print::PROFILE)?, bstd::fs::Content::bytes(text), bstd::fs::Durable::No)?;
    Ok(())
}

/// Publish the board's live grouped activity (`build.rs`, `fetch`, `proc-macro`, Blaze checking)
/// and aggregate progress counts to `live`.
pub(crate) fn report_live(board: &Board<'_>, now: u64, live: &(dyn blaze_screen::Watch + Sync)) {
    let mut done_sum = 0u64;
    let mut known_sum = 0u64;
    for counts in board.counts.iter() {
        let done = counts.done.load(Ordering::Acquire);
        let known = counts.known.load(Ordering::Acquire).max(done);
        done_sum = done_sum.saturating_add(done);
        known_sum = known_sum.saturating_add(known);
    }
    let grouped = board.grouped();
    if known_sum == 0 && grouped.active == 0 && board.stage().is_none() {
        return;
    }
    let progress = (known_sum > 0).then(|| {
        let done = u32::try_from(done_sum).unwrap_or(u32::MAX);
        let total = u32::try_from(known_sum).unwrap_or(u32::MAX);
        (done, total)
    });

    // Build concise Blaze sub-stage hint (e.g. `names / bodies`).
    let mut hint_buf = FixedString::<{ blaze_screen::HINT_BYTES }>::new();
    let mut hint_parts = 0u8;
    for stage in Stage::ALL {
        let bit = u8::try_from(stage.index()).unwrap_or(7);
        if bit < 8 && (grouped.blaze_stages & (1u8 << bit)) != 0 {
            if hint_parts > 0 {
                let _ = hint_buf.write_str(" / ");
            }
            let _ = hint_buf.write_str(stage_hint_name(stage));
            hint_parts += 1;
        }
    }
    let hint = (!hint_buf.as_str().is_empty()).then_some(hint_buf.as_str());

    // Build per-category active item slices on the stack (zero heap allocation).
    const EMPTY_ITEM: blaze_screen::ActiveItemRef<'static> =
        blaze_screen::ActiveItemRef { label: "", elapsed_ms: 0 };
    let mut item_storage = [[EMPTY_ITEM; blaze_screen::MAX_GROUP_ITEMS]; blaze_screen::WORK_KINDS];
    let mut item_lens = [0usize; blaze_screen::WORK_KINDS];
    for ((dst_row, dst_len), src_row) in item_storage.iter_mut().zip(item_lens.iter_mut()).zip(grouped.top.iter()) {
        let mut len = 0usize;
        for slot in src_row.iter().flatten() {
            let label = slot.label.as_str();
            if label.is_empty() {
                continue;
            }
            if let Some(cell) = dst_row.get_mut(len) {
                *cell = blaze_screen::ActiveItemRef {
                    label,
                    elapsed_ms: now.saturating_sub(slot.from_ns) / 1_000_000,
                };
                len += 1;
            }
        }
        *dst_len = len;
    }

    let mut groups_buf = [blaze_screen::ActiveGroupRef {
        kind: blaze_screen::WorkKind::Blaze,
        items: &[],
        extra: 0,
    }; blaze_screen::WORK_KINDS];
    let mut group_count = 0usize;
    for (((kind, row), &len), &total) in blaze_screen::WorkKind::ALL
        .into_iter()
        .zip(item_storage.iter())
        .zip(item_lens.iter())
        .zip(grouped.counts.iter())
    {
        if len == 0 {
            continue;
        }
        let Some(items) = row.get(..len) else { continue };
        let Some(dst) = groups_buf.get_mut(group_count) else { break };
        let extra = u16::try_from(total.saturating_sub(u32::try_from(len).unwrap_or(0))).unwrap_or(u16::MAX);
        *dst = blaze_screen::ActiveGroupRef { kind, items, extra };
        group_count += 1;
    }

    let groups = groups_buf.get(..group_count).unwrap_or(&[]);
    live.see(blaze_screen::Step::Activity(blaze_screen::Activity {
        verb: "checking",
        progress,
        active: grouped.active,
        hint,
        groups,
    }));
}

/// `work` with the watcher beside it when a signal can ask, a line is due, or a live progress sink
/// is attached; the watcher ends and is joined on every path, an unwind too. A stop the workers
/// do not reach in [`STOP_BOUND`] is written from here and ends the process (`Exit::StopStalled`).
/// A plant starts no watcher: its requests are the driver's at its scheduling points, so a planted
/// run stays one deterministic thread on one worker.
#[expect(clippy::too_many_arguments, reason = "the watcher's edge: each argument one specific capability or sink")]
pub(crate) fn watched<R>(
    cx: &Cx,
    host: Option<&Host>,
    root: &Dir<ReadWrite>,
    err: &Out,
    clock: &Clock,
    board: &Board<'_>,
    every: Option<Duration>,
    live: Option<&(dyn blaze_screen::Watch + Sync)>,
    work: impl FnOnce() -> R,
) -> Result<R, StatusError> {
    if board.asked.is_none() && every.is_none() && live.is_none() {
        return Ok(work());
    }
    let spawner = cx.spawner();
    struct Over<'b, 'r>(&'b Board<'r>, &'b Signal);
    impl Drop for Over<'_, '_> {
        fn drop(&mut self) {
            self.0.finish();
            self.1.notify_all();
        }
    }
    let woken = Signal::new("engine.status.tick");
    let parked = Lock::new("engine.status.park", ());
    // The watcher's context is made on its thread (as run_line.rs's ticker's); its failure is
    // the command's after the work (rule 3: never a quiet missing watcher).
    let failed: Lock<Option<bstd::Stopped>> = Lock::new("engine.status.failed", None);
    let got = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .spawn_scoped(scope, || {
                let cx = match spawner.cx(Duration::MAX, WATCH_SCRATCH) {
                    Ok(cx) => cx,
                    Err(why) => {
                        *failed.lock() = Some(why);
                        return;
                    }
                };
                let mut watch = Watch::new(every);
                let mut guard = parked.lock();
                while !board.over.load(Ordering::Acquire) {
                    let now = board.now(clock);
                    if let Some(sink) = live {
                        report_live(board, now, sink);
                    }
                    let act = watch.look(now, board.stopping(), board.take_status(), board.claimed.load(Ordering::Acquire) != 0);
                    act_on(&cx, host, root, err, board, act, now);
                    // A bounded wait (rule 15, 18): the next look in a tick, or at the work's end.
                    let _span = cx.span("engine status: the next look");
                    woken.wait_for(&mut guard, TICK);
                }
            })
            .map_err(|why| StatusError::Watcher { kind: why.kind() })?;
        let over = Over(board, &woken);
        let got = work();
        drop(over);
        Ok(got)
    })?;
    match failed.lock().take() {
        Some(why) => Err(StatusError::Context(why)),
        None => Ok(got),
    }
}

/// One act's writes. A write that fails is said on stderr once and the check goes on (the
/// status is a report; the check's own result is the command's).
fn act_on(cx: &Cx, host: Option<&Host>, root: &Dir<ReadWrite>, err: &Out, board: &Board<'_>, act: Act, now: u64) {
    let mut text = FixedString::<TEXT>::new();
    let heading = match act {
        Act::Nothing => return,
        Act::Line => {
            let _ = line(board, now, &mut text);
            let _ = err.write_all(cx, text.as_str().as_bytes()).and_then(|()| err.flush(cx));
            return;
        }
        Act::Status(by) => Heading::Asked { by },
        Act::Stopped(by) => {
            if !board.claim(Claim::Watcher) {
                return;
            }
            Heading::Stopped { by, stuck: true }
        }
    };
    let _ = write_status(board, heading, now, &mut text);
    if text.is_refused() {
        let _ = writeln!(text, "(the status passed {TEXT} bytes: cut here)");
    }
    if let Err(why) = put(cx, root, err, text.as_str().as_bytes()) {
        let mut said = FixedString::<512>::new();
        let _ = writeln!(said, "engine-check: the status was not written: {why}");
        let _ = err.write_all(cx, said.as_str().as_bytes());
    }
    if let Heading::Stopped { .. } = heading {
        match host {
            Some(host) => host.exit(blaze_core::exit::Exit::StopStalled.code()),
            None => {
                let mut said = FixedString::<512>::new();
                let _ = writeln!(said, "engine-check: stopped, and the process could not be ended: missing host capability");
                let _ = err.write_all(cx, said.as_str().as_bytes());
            }
        }
    }
}

/// `d` in ns.
fn ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Seconds to a tenth, from ns.
struct Secs(u64);

impl fmt::Display for Secs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = self.0 / 100_000_000;
        write!(f, "{}.{}", tenths / 10, tenths % 10)
    }
}

/// Gigabytes (10^9) to a tenth.
struct Gb(u64);

impl fmt::Display for Gb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = self.0 / 100_000_000;
        write!(f, "{}.{}", tenths / 10, tenths % 10)
    }
}
