//! Every crash point of a short run, instead of one drawn: test.md, "End state: the search
//! itself", piece S5 (decision D95).
//!
//! # Every cut
//!
//! [`Sim::crash`] draws ONE of the disks a power cut may leave. For a short sequence of file
//! effects the space is small enough to try them all: [`every_cut`] runs a program once under
//! a [`Sim`], one effect at a time ([`Steps::step`]), and keeps the disk as the simulation
//! holds it at each moment between two completions (before the first effect, after each
//! write, between a write and the sync that covers it, after each sync). A crash while an
//! effect is in flight is a crash at the moment before it completes: every effect [`Steps`]
//! takes changes the disk only as it completes (sim.rs: `Sim::complete`), and a write from
//! memory, which lands sectors while in flight (sim.rs: `Sim::land_some`), is refused by name
//! ([`Refused::NotEnumerated`]). Of each moment it enumerates every disk the crash may leave,
//! dedupes them by their bytes, rebuilds each on a fresh [`Sim`] and calls the caller's
//! recovery and invariant on it with the [`CrashPoint`] that names it.
//!
//! What a crash may leave is exactly what `Sim::crash_cut` and `Sim::crash_with` (sim.rs) can
//! draw, no more and no less, mirrored step by step here:
//! - each file, in the order of the names, keeps its first `whole` changes not synced
//!   (`kept`, any of `0..=all`) and may have landed in part up to `upto` (any of
//!   `whole..=all`): every pair is enumerated ([`pairs_of`]);
//! - the barriers' promise raises `whole` (and `upto` with it) to what a barrier of an earlier
//!   epoch than the newest change kept covers, a change of a name the crash does not undo
//!   counted as kept, in rounds, until nothing moves: not a mirror but the same function
//!   (`sim.rs`'s `keep_ordered`), the same planted bug honoured ([`close`]); stamps not yet
//!   given are the epoch now, as `Sim::stamp_epochs` gives them;
//! - where `upto > whole` the file's length is ANY value between the two files' lengths (the
//!   random crash draws the longer, the shorter, or one uniform between), and the sectors
//!   under it are ANY vector of landed bits (the random crash draws all landed one time in
//!   three, else each bit on its own), the bytes made by the disk world's own
//!   [`crate::world::disk::mixed`], as `crash_with` makes them; so the prefixes of a write of
//!   several sectors, each later sector surviving alone, and every mix between are all here;
//! - each change of a name not synced is undone or not, a later one sharing a name with one
//!   undone undone with it, and the undoing done newest first as `crash_with` does; a
//!   directory's making undone takes every file and directory under it, a directory's removal
//!   undone brings it back empty (`sim/host/dir_names.rs`). A disk left names its directories
//!   ([`AfterCrash::dirs`]), so a folder a write made and a crash took is told apart from one
//!   kept empty.
//!
//! HOW THE TWO ARE KNOWN TO AGREE: the mirror above is one reading of the code; the proof is
//! the test `the_enumeration_is_the_random_crash_s_set`, a property on `blaze_sim::check`: for
//! drawn journals of appends, syncs, barriers and directory syncs over two files, at every
//! moment, every disk `Sim::crash` leaves over many seeds (run on a copy of the moment's disk)
//! is in the enumeration, and every disk of the enumeration is left by some seed. A change to
//! `crash_cut` that this file does not follow fails it.
//!
//! Bounded: the caller passes a cap on crash points; the count is made before any disk is
//! built, and past the cap the answer is [`Refused::TooMany`] naming the count, never a
//! sample. The order is deterministic: moments in order; in a moment, the choices of names
//! undone grouped by the newest epoch of the changes of names they keep, ascending (none
//! first, [`groups`]); in a group, the cut tuples sorted, then the lengths ascending, the
//! landed bits counted up from none, then the names undone sorted. The random crash stays for
//! long sequences.
//!
//! # What `Sim` injects (checked against the code, 2026-11-16)
//!
//! | Fault | Where | |
//! |---|---|---|
//! | a short write | sim.rs: `Sim::complete` (a failed append at [`Faults::fail_one_in`] keeps a drawn prefix); `Sim::take_room` (out of room, an append keeps the part that fits) | exists |
//! | a short read | sim.rs: `Sim::short_reads` (reads into memory end short); a socket receive: `short-receive` below | exists; added |
//! | a failed sync | sim.rs: `Sim::fail_syncs`; `Sim::complete` at [`Faults::fail_one_in`] | exists |
//! | out of room | sim.rs: `Sim::fill`, `Sim::take_room` | exists |
//! | a torn write | sim.rs: `Sim::crash_cut`, `Sim::crash_with` (by sector, `world::disk::mixed`); `Sim::land_some` (a failed or in-flight write from memory) | exists |
//! | a completion delivered late | sim.rs: `latency` up to [`Faults::max_latency`]; `Sim::hold_syncs`, `Sim::hold_reads`, `Sim::slow_writes`, `Sim::hold_sends`, `Sim::hold_lists` | exists |
//! | a completion delivered twice | none for a completion; host.rs: `Datagrams` doubles a datagram | not added: no kernel completes one request twice (a ring's completion is posted once per submission); a doubled datagram, which is real, exists |
//! | completions reordered | sim.rs: `Sim::poll` delivers what is ready in a shuffled order | exists |
//! | a spurious wake | sim.rs: `Sim::wait` (`Once::Spurious` and `Once::Interrupted` at [`Faults::interrupt_one_in`]); wake.rs: wakes landing before the look or the park | exists |
//! | the clock jumping forward | sim.rs: `Sim::advance` (asked by a test); `clock-jump` below (drawn) | exists; added |
//! | the clock standing still | sim.rs: `Sim::poll` hands every completion ready at one instant in one call; `latency` may draw zero | exists |
//! | the clock at its greatest value | sim.rs: `Sim::advance` saturates; the wheel's end (`pending`) holds what is due past it; tests time out at `Duration::MAX` | exists, asked by a test; not drawn: no monotonic clock reaches it (it counts from boot) |
//!
//! The kinds added, each opt-in by its name in the list a caller passes ([`Sim::inject`],
//! [`KINDS`]; a swarm switches them per case): an existing caller never asks, and a kind off
//! draws no number, so every existing seed's run is the run it was.
//! - `short-receive`: a receive on a socket takes a drawn part (at least one byte) of what has
//!   come, the rest left for the next receive. read(2) on this machine: "The system guarantees
//!   to read the number of bytes requested if the descriptor references a normal file that has
//!   that many bytes left before the end-of-file, but in no other case." The same page is why a
//!   short read of a normal file mid-file is NOT added: it guarantees the whole. write(2):
//!   "When using non-blocking I/O on objects, such as sockets, that are subject to flow
//!   control, write and writev may write fewer bytes than requested": a send's contract is the
//!   whole ([`crate::Effect::Send`]), so the driver writes the rest; what its peer sees is a
//!   frame in parts, which this kind is.
//! - `clock-jump`: at a poll, one in four, the virtual clock jumps forward by a drawn time up
//!   to an hour, as after the machine slept. clock_gettime(3) on this machine:
//!   "CLOCK_MONOTONIC clock that increments monotonically, tracking the time since an
//!   arbitrary point, and will continue to increment while the system is asleep."
//!
//! fsync(2) on this machine ("the drive itself may not physically write the data to the
//! platters for quite some time and it may be written in an out-of-order sequence ... later
//! writes may be present, while earlier writes are not") is the tear above, which exists: a
//! later sector surviving an earlier one.
//!
//! Allocation: as the rest of `sim.rs`, the simulation's own memory is named `Global` in the
//! code (it is a test driver; its row in the ratchet does not rise: this file names no `std`
//! or `alloc` path, it takes `Global` from `super`).

use core::fmt;

use blaze_own::Share;

use super::host::Plant;
use super::{Duration, Global, NameChange, Named, Op, Rng, Sim, SimFile, keep_ordered};
use crate::world::disk::{SECTOR, mixed, sectors};
use crate::{Completion, Done, Driver, Effect, FileName, Tag, wait::Deadline};

/// The fault kinds a caller may switch on by name ([`Sim::inject`]), in a stable order.
pub const KINDS: &[&str] = &["short-receive", "clock-jump"];

/// The most polls one step waits for its effect's completion before it is refused as stalled
/// (rule bounded-progress): far over what any effect under `Sim` takes.
const POLLS_MOST: u32 = 1 << 16;
/// The most cut tuples (one pair of changes kept whole and landed in part per file) one moment
/// may have before the barriers close them: past it the moment is refused, never sampled.
const TUPLES_MOST: u128 = 1 << 20;
/// The most changes of names not synced one moment may have: each doubles the choices.
const NAMES_MOST: usize = 16;
/// The chance, one in this many, that a poll with `clock-jump` on starts with a jump.
const JUMP_ONE_IN: u64 = 4;
/// The longest jump `clock-jump` draws, in milliseconds: an hour asleep.
const JUMP_MOST_MS: u64 = 3_600_000;

/// Which opt-in fault kinds a [`Sim`] injects ([`KINDS`]); none unless asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inject {
    bits: u8,
}

/// One opt-in fault kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `short-receive`: a socket receive takes part of what came.
    ShortReceive,
    /// `clock-jump`: the clock jumps forward at a poll, as after a sleep.
    ClockJump,
}

impl Kind {
    fn bit(self) -> u8 {
        match self {
            Kind::ShortReceive => 1,
            Kind::ClockJump => 2,
        }
    }

    fn named(name: &str) -> Option<Kind> {
        match name {
            "short-receive" => Some(Kind::ShortReceive),
            "clock-jump" => Some(Kind::ClockJump),
            _ => None,
        }
    }
}

impl Inject {
    /// No kind on: every existing caller's.
    pub const NONE: Inject = Inject { bits: 0 };

    /// Whether `kind` is on.
    pub fn has(self, kind: Kind) -> bool {
        self.bits & kind.bit() != 0
    }
}

/// A kind name [`Sim::inject`] does not know: refused by name, never ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownKind {
    pub name: &'static str,
}

impl fmt::Display for UnknownKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no fault kind named {:?} in the simulation (it has {:?}; test.md S5)", self.name, KINDS)
    }
}

impl Sim {
    /// The opt-in fault kinds named in `kinds` ([`KINDS`]) are on from now on, beside the ones
    /// on already. A name it does not know is refused, naming it, and nothing is switched.
    pub fn inject(&mut self, kinds: &[&'static str]) -> Result<(), UnknownKind> {
        let mut bits = self.inject.bits;
        for name in kinds.iter().copied() {
            match Kind::named(name) {
                Some(kind) => bits |= kind.bit(),
                None => return Err(UnknownKind { name }),
            }
        }
        self.inject.bits = bits;
        Ok(())
    }
}

/// How many bytes of `all` that came a receive takes: all of them, or with `short-receive`
/// on, one time in two a drawn part of at least one byte. Draws nothing with the kind off.
pub(super) fn short_receive(inject: Inject, rng: &mut Rng, all: usize) -> usize {
    if !inject.has(Kind::ShortReceive) || all < 2 || !rng.one_in(2) {
        return all;
    }
    1 + rng.below(all as u64 - 1) as usize
}

/// With `clock-jump` on, one poll in [`JUMP_ONE_IN`] starts with the clock moved forward by a
/// drawn time up to an hour, as `Sim::advance` moves it. Draws nothing with the kind off.
pub(super) fn jump_clock(sim: &mut Sim) {
    if !sim.inject.has(Kind::ClockJump) || !sim.rng.one_in(JUMP_ONE_IN) {
        return;
    }
    let by = Duration::from_millis(1 + sim.rng.below(JUMP_MOST_MS));
    sim.advance(by);
}

/// One file a crash left: its name, whether it is a symbolic link, and its bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Left {
    pub name: FileName,
    pub link: bool,
    pub bytes: Vec<u8>,
}

/// A disk a crash left: every file there, by name, and every directory there but the root,
/// sorted (a directory a crash keeps is there empty or not, as the simulation's are).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AfterCrash {
    pub files: Vec<Left>,
    pub dirs: Vec<FileName>,
}

impl AfterCrash {
    /// The bytes of `name`, if it is there.
    pub fn file(&self, name: &FileName) -> Option<&[u8]> {
        self.files.binary_search_by(|it| it.name.cmp(name)).ok().map(|at| &self.files[at].bytes[..])
    }

    /// The disk `sim` holds now, every change made: after [`Sim::crash`], what the crash left.
    pub fn of(sim: &Sim) -> AfterCrash {
        let mut files: Vec<Left> = Vec::with_capacity_in(sim.files.len(), Global);
        for (name, file) in sim.files.iter() {
            // clone: a refcount; the bytes are the disk's copy, which is what is asked.
            files.push(Left { name: name.share(), link: file.link, bytes: file.contents() });
        }
        files.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let mut dirs: Vec<FileName> = Vec::with_capacity_in(sim.host.made.len(), Global);
        // clone: refcounts; `made` is sorted.
        dirs.extend(sim.host.made.iter().cloned());
        AfterCrash { files, dirs }
    }
}

/// Of a file's changes not synced, what a cut landed past those kept whole.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tear {
    /// Its first `upto` changes made is the file the landed sectors are taken from.
    pub upto: usize,
    /// The file's length after the crash.
    pub len: usize,
    /// Each sector under `len`, in order: the new bytes, or the old.
    pub landed: Vec<bool>,
}

/// What a crash point keeps of one file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileCut {
    pub name: FileName,
    /// Its changes not synced at the moment.
    pub changes: usize,
    /// How many of them, from the first, the crash keeps whole.
    pub whole: usize,
    /// What landed of the ones after, if any may have.
    pub tear: Option<Tear>,
}

/// One crash point, by name: the moment, and what the crash kept there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashPoint {
    /// After how many steps of the run (0: before the first).
    pub after: usize,
    /// The step that ended the moment, as the program named it ("start" for none).
    pub step: &'static str,
    /// Of each file, in the order of the names.
    pub files: Vec<FileCut>,
    /// Of each change of a name not synced, in its order, whether the crash undid it.
    pub undone: Vec<bool>,
}

impl fmt::Display for CrashPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "after step {} ({:?}):", self.after, self.step)?;
        for cut in &self.files {
            write!(f, " {}: {} of {} changes whole", cut.name, cut.whole, cut.changes)?;
            if let Some(tear) = &cut.tear {
                write!(f, ", then up to change {} torn to {} bytes, sectors landed [", tear.upto, tear.len)?;
                for (i, landed) in tear.landed.iter().enumerate() {
                    let sep = if i == 0 { "" } else { " " };
                    write!(f, "{sep}{}", u8::from(*landed))?;
                }
                write!(f, "]")?;
            }
            write!(f, ";")?;
        }
        let undone = self.undone.iter().filter(|it| **it).count();
        match undone {
            0 => write!(f, " no change of a name undone"),
            n => write!(f, " {n} of {} changes of names undone", self.undone.len()),
        }
    }
}

/// What [`every_cut`] refuses, each naming the moment or step and the rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// More crash points than the cap: the count, made before any disk was built.
    TooMany { points: u128, cap: usize },
    /// A moment with more cut tuples than [`TUPLES_MOST`] to close.
    TooManyTuples { after: usize, tuples: u128 },
    /// A moment with more changes of names not synced than [`NAMES_MOST`].
    TooManyNames { after: usize, names: usize },
    /// An effect whose crash in flight lands what this does not enumerate.
    NotEnumerated { step: &'static str, effect: &'static str },
    /// A moment with effects in flight, which a crash may land.
    InFlight { after: usize, effects: usize },
    /// A moment with a mapping for writing held, which a crash folds.
    MappedForWriting { after: usize },
    /// A step's poll told a completion of another effect, or more than one.
    NotOneAtATime { step: &'static str, tag: Tag },
    /// A step's effect did not complete within [`POLLS_MOST`] polls.
    Stalled { step: &'static str, polls: u32 },
    /// A symbolic link whose target is not text, which the rebuilt disk cannot hold.
    LinkNotText { name: FileName },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "every_cut (test.md S5)";
        match self {
            Refused::TooMany { points, cap } => {
                write!(f, "too many: {points} crash points over the cap of {cap}; {rule}: the random crash is for a run this long")
            }
            Refused::TooManyTuples { after, tuples } => {
                write!(f, "too many: {tuples} cut tuples at the moment after step {after}, over {TUPLES_MOST}; {rule}")
            }
            Refused::TooManyNames { after, names } => {
                write!(f, "too many: {names} changes of names not synced after step {after}, over {NAMES_MOST}; {rule}")
            }
            Refused::NotEnumerated { step, effect } => {
                write!(f, "step {step:?}: a {effect} lands sectors while in flight, which {rule} does not enumerate")
            }
            Refused::InFlight { after, effects } => {
                write!(f, "after step {after}: {effects} effects in flight, which a crash may land; {rule} takes one at a time")
            }
            Refused::MappedForWriting { after } => {
                write!(f, "after step {after}: a mapping for writing is held, which a crash folds; {rule} does not enumerate it")
            }
            Refused::NotOneAtATime { step, tag } => {
                write!(f, "step {step:?}: told {tag:?}, not its own effect alone; {rule} takes one effect at a time")
            }
            Refused::Stalled { step, polls } => {
                write!(f, "step {step:?}: its effect did not complete in {polls} polls (rule bounded-progress)")
            }
            Refused::LinkNotText { name } => {
                write!(f, "{name}: a symbolic link whose target is not text, which {rule} cannot rebuild")
            }
        }
    }
}

/// What a moment's crash may start from: a copy of the simulation's disk.
struct DiskCopy {
    /// Every file, by name.
    files: Vec<(FileName, SimFile)>,
    /// Every directory there but the root, sorted (`Host::made`).
    dirs: Vec<FileName>,
    /// The changes of names not synced, oldest first, each with its epoch.
    names: Vec<NameChange>,
    /// The epoch now: the stamp of every change not stamped yet.
    epoch: u64,
    /// The planted bug of the barriers' promise, honoured as `crash_cut` honours it.
    barriers_not_kept: bool,
}

fn copy_op(op: &Op) -> Op {
    // clone: a copy of the disk at a moment, which is what is asked.
    match op {
        Op::Append(more) => Op::Append(more.clone()),
        Op::At(at, more) => Op::At(*at, more.clone()),
        Op::Sectors(at, more) => Op::Sectors(*at, more.clone()),
        Op::Cut(to) => Op::Cut(*to),
    }
}

fn copy_file(file: &SimFile) -> SimFile {
    let mut ops = Vec::with_capacity_in(file.ops.len(), Global);
    for op in &file.ops {
        ops.push(copy_op(op));
    }
    // clone: a copy of the disk at a moment, which is what is asked.
    SimFile {
        durable: file.durable.clone(),
        ops,
        mode: file.mode,
        link: file.link,
        back: file.back,
        stamps: file.stamps.clone(),
        barriers: file.barriers.clone(),
        blocks: file.blocks.clone(),
    }
}

fn copy_named(change: &NameChange) -> NameChange {
    NameChange { named: copy_change_of_name(&change.named), epoch: change.epoch }
}

fn copy_change_of_name(named: &Named) -> Named {
    // clone: refcounts of names and a copy of the disk at a moment.
    match named {
        Named::Put { file, was } => Named::Put { file: file.share(), was: was.as_ref().map(copy_file) },
        Named::Gone { file, was } => Named::Gone { file: file.share(), was: copy_file(was) },
        Named::Moved { from, to, was } => {
            Named::Moved { from: from.share(), to: to.share(), was: was.as_ref().map(copy_file) }
        }
        Named::MadeDir { dir } => Named::MadeDir { dir: dir.share() },
        Named::RemovedDir { dir } => Named::RemovedDir { dir: dir.share() },
    }
}

impl DiskCopy {
    fn of(sim: &Sim) -> DiskCopy {
        let mut files: Vec<(FileName, SimFile)> = Vec::with_capacity_in(sim.files.len(), Global);
        for (name, file) in sim.files.iter() {
            // clone: a refcount.
            files.push((name.share(), copy_file(file)));
        }
        files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut names = Vec::with_capacity_in(sim.names.len(), Global);
        for named in &sim.names {
            names.push(copy_named(named));
        }
        let mut dirs: Vec<FileName> = Vec::with_capacity_in(sim.host.made.len(), Global);
        // clone: refcounts.
        dirs.extend(sim.host.made.iter().cloned());
        DiskCopy { files, dirs, names, epoch: sim.epoch, barriers_not_kept: sim.planted(Plant::BarriersNotKept) }
    }

    /// The copy put into `sim`, a fresh one: its files, its directories and its names not
    /// synced, so that its own [`Sim::crash`] draws from the moment's disk.
    fn restore(&self, sim: &mut Sim) {
        for (name, file) in &self.files {
            // clone: a refcount.
            sim.files.insert(name.share(), copy_file(file));
        }
        // clone: refcounts.
        sim.host.made.extend(self.dirs.iter().cloned());
        for named in &self.names {
            sim.names.push(copy_named(named));
        }
        sim.epoch = self.epoch;
    }

    /// Each file's stamps as `Sim::stamp_epochs` leaves them at a crash.
    fn stamps(&self) -> Vec<Vec<u64>> {
        let mut all = Vec::with_capacity_in(self.files.len(), Global);
        for (_, file) in &self.files {
            // clone: the crash's own copy, as `stamp_epochs` extends the file's.
            let mut stamps = file.stamps.clone();
            let missing = file.ops.len().saturating_sub(stamps.len());
            for _ in 0..missing {
                stamps.push(self.epoch);
            }
            all.push(stamps);
        }
        all
    }
}

/// One moment of the run: after how many steps, the last step's name, and its disk.
struct Moment {
    after: usize,
    step: &'static str,
    disk: DiskCopy,
}

/// A program's run, one effect at a time, with the disk kept at each moment.
pub struct Steps<'s> {
    sim: &'s mut Sim,
    moments: Vec<Moment>,
    done: Vec<Completion>,
}

impl Steps<'_> {
    /// The simulation, for a look or a change of the world between steps (a change made here
    /// is in the next moment's disk).
    pub fn sim(&mut self) -> &mut Sim {
        &mut *self.sim
    }

    /// `effect` given to the simulation alone and waited for: its completion. The moment after
    /// it is kept. A write from memory and a mapping for writing are refused by name.
    pub fn step(&mut self, what: &'static str, effect: Effect) -> Result<Done, Refused> {
        match &effect {
            Effect::WriteFrom { .. } => return Err(Refused::NotEnumerated { step: what, effect: "write from memory" }),
            Effect::MapWrite { .. } => return Err(Refused::NotEnumerated { step: what, effect: "mapping for writing" }),
            _ => {}
        }
        let tag = effect.tag();
        let mut effects = Vec::with_capacity_in(1, Global);
        effects.push(effect);
        self.sim.submit(&mut effects);
        for _ in 0..POLLS_MOST {
            self.sim.poll(&mut self.done, Deadline::NEVER);
            if let Some(completion) = self.done.pop() {
                if completion.tag != tag || !self.done.is_empty() {
                    return Err(Refused::NotOneAtATime { step: what, tag: completion.tag });
                }
                self.keep(what)?;
                return Ok(completion.done);
            }
            if self.sim.in_flight() == 0 {
                break;
            }
        }
        Err(Refused::Stalled { step: what, polls: POLLS_MOST })
    }

    /// The disk now kept as the next moment, or refused where a crash now does what this does
    /// not enumerate.
    fn keep(&mut self, step: &'static str) -> Result<(), Refused> {
        let after = self.moments.len();
        if !self.sim.pending.is_empty() {
            return Err(Refused::InFlight { after, effects: self.sim.pending.len() });
        }
        if !self.sim.drawn.is_empty() {
            return Err(Refused::MappedForWriting { after });
        }
        for (name, file) in self.sim.files.iter() {
            if file.link && core::str::from_utf8(&file.contents()).is_err() {
                // clone: a refcount, for the error.
                return Err(Refused::LinkNotText { name: name.share() });
            }
        }
        self.moments.push(Moment { after, step, disk: DiskCopy::of(self.sim) });
        Ok(())
    }
}

/// The bounds of a sweep: the cap on crash points, and the seed of each fresh simulation the
/// recovery runs on (rule nothing-ambient: the caller's).
#[derive(Clone, Copy, Debug)]
pub struct Sweep {
    pub cap: usize,
    pub recovery_seed: u64,
}

/// What a sweep did.
#[derive(Debug)]
pub struct Swept<E> {
    /// Moments of the run (one more than its steps).
    pub moments: usize,
    /// Crash points enumerated: every cut at every moment.
    pub cuts: u128,
    /// The distinct disks among them: the recovery's runs, one for each.
    pub points: usize,
    /// Each run of the recovery that failed: the first crash point that left the disk, and
    /// the recovery's error.
    pub failures: Vec<(CrashPoint, E)>,
}

/// Every crash point of `program`'s run on `sim`: the run once, one effect at a time
/// ([`Steps::step`]); then, of every disk a crash at any moment may leave (the module's doc),
/// each distinct one rebuilt on a fresh `Sim` and `recover` called on it with the first point
/// that left it. Past `sweep.cap` crash points, [`Refused::TooMany`] with the count; a run
/// whose crash may do what this does not enumerate is refused by name.
pub fn every_cut<E>(
    sim: &mut Sim,
    sweep: Sweep,
    program: impl FnOnce(&mut Steps<'_>) -> Result<(), Refused>,
    mut recover: impl FnMut(&mut Sim, &CrashPoint) -> Result<(), E>,
) -> Result<Swept<E>, Refused> {
    let moments = record(sim, program)?;
    let mut cuts: u128 = 0;
    for moment in &moments {
        cuts = cuts.saturating_add(count(moment)?);
    }
    if cuts > sweep.cap as u128 {
        return Err(Refused::TooMany { points: cuts, cap: sweep.cap });
    }
    let mut seen: Vec<AfterCrash> = Vec::new_in(Global);
    let mut failures = Vec::new_in(Global);
    for moment in &moments {
        for (point, disk) in enumerate(moment)? {
            let Err(at) = seen.binary_search(&disk) else { continue };
            let mut fresh = rebuilt(&disk, sweep.recovery_seed);
            if let Err(error) = recover(&mut fresh, &point) {
                failures.push((point, error));
            }
            seen.insert(at, disk);
        }
    }
    Ok(Swept { moments: moments.len(), cuts, points: seen.len(), failures })
}

/// The run of `program` on `sim`, each moment kept, the first before any step.
fn record(sim: &mut Sim, program: impl FnOnce(&mut Steps<'_>) -> Result<(), Refused>) -> Result<Vec<Moment>, Refused> {
    let mut steps = Steps { sim, moments: Vec::new_in(Global), done: Vec::new_in(Global) };
    steps.keep("start")?;
    program(&mut steps)?;
    Ok(steps.moments)
}

/// A fresh simulation holding `disk`, every file and directory of it durable.
fn rebuilt(disk: &AfterCrash, seed: u64) -> Sim {
    let mut sim = Sim::new(seed, super::Faults::default());
    for left in &disk.files {
        // clone: the rebuilt disk's own bytes.
        let mut file = SimFile::whole(left.bytes.clone());
        file.link = left.link;
        // clone: a refcount.
        sim.files.insert(left.name.share(), file);
    }
    // clone: refcounts; sorted, as `made` is kept.
    sim.host.made.extend(disk.dirs.iter().cloned());
    sim
}

/// Every (changes kept whole, changes landed in part) a crash draws for a file with `all`
/// changes not synced, as `crash_cut` draws them: kept any of `0..=all`, upto any of
/// `kept..=all`.
fn pairs_of(all: usize) -> Vec<(usize, usize)> {
    let mut pairs = Vec::with_capacity_in((all + 1) * (all + 2) / 2, Global);
    for kept in 0..=all {
        for upto in kept..=all {
            pairs.push((kept, upto));
        }
    }
    pairs
}

/// The barriers' promise on one cut tuple, by the rule `crash_cut` keeps it by
/// ([`keep_ordered`], the rule's one home), with `names` the newest epoch of the changes of
/// names the crash keeps.
fn close(disk: &DiskCopy, stamps: &[Vec<u64>], names: Option<u64>, cut: &mut [(usize, usize)]) {
    let mut files: Vec<(&[u64], &[(usize, u64)])> = Vec::with_capacity_in(disk.files.len(), Global);
    files.extend(disk.files.iter().zip(stamps).map(|((_, file), stamps)| (&stamps[..], &file.barriers[..])));
    keep_ordered(&files, names, disk.barriers_not_kept, cut);
}

/// The newest epoch of the changes of names a crash that undoes `undone` keeps: none where it
/// keeps none.
fn names_kept(disk: &DiskCopy, undone: &[bool]) -> Option<u64> {
    disk.names.iter().zip(undone).filter(|(_, undone)| !**undone).map(|(change, _)| change.epoch).max()
}

/// The crash points of a moment in groups: the choices of names undone ([`undone_sets`]) by
/// the newest epoch of the changes of names they keep, ascending (none first), each group with
/// every cut tuple the barriers close to under that epoch ([`closed_tuples`]). The closing
/// depends on the names undone only through that epoch, so each tuple is closed once a group.
fn groups(moment: &Moment) -> Result<Vec<(Vec<Vec<(usize, usize)>>, Vec<Vec<bool>>)>, Refused> {
    let mut by_newest: Vec<(Option<u64>, Vec<Vec<bool>>)> = Vec::new_in(Global);
    for set in undone_sets(moment)? {
        let newest = names_kept(&moment.disk, &set);
        match by_newest.binary_search_by(|(it, _)| it.cmp(&newest)) {
            Ok(at) => by_newest[at].1.push(set),
            Err(at) => {
                let mut sets = Vec::new_in(Global);
                sets.push(set);
                by_newest.insert(at, (newest, sets));
            }
        }
    }
    let mut out = Vec::with_capacity_in(by_newest.len(), Global);
    for (newest, sets) in by_newest {
        out.push((closed_tuples(moment, newest)?, sets));
    }
    Ok(out)
}

/// Every cut tuple of a moment after the barriers closed it, the changes of names kept of an
/// epoch up to `names`, sorted, each once.
fn closed_tuples(moment: &Moment, names: Option<u64>) -> Result<Vec<Vec<(usize, usize)>>, Refused> {
    let disk = &moment.disk;
    let mut lists = Vec::with_capacity_in(disk.files.len(), Global);
    let mut tuples: u128 = 1;
    for (_, file) in &disk.files {
        let pairs = pairs_of(file.ops.len());
        tuples = tuples.saturating_mul(pairs.len() as u128);
        lists.push(pairs);
    }
    if tuples > TUPLES_MOST {
        return Err(Refused::TooManyTuples { after: moment.after, tuples });
    }
    let stamps = disk.stamps();
    let mut closed: Vec<Vec<(usize, usize)>> = Vec::new_in(Global);
    let mut index = Vec::with_capacity_in(lists.len(), Global);
    index.resize(lists.len(), 0usize);
    for _ in 0..tuples {
        let mut cut = Vec::with_capacity_in(lists.len(), Global);
        for (list, at) in lists.iter().zip(&index) {
            cut.push(list[*at]);
        }
        close(disk, &stamps, names, &mut cut);
        closed.push(cut);
        // The next tuple: the last file's pair moves fastest.
        let mut file = lists.len();
        while file > 0 {
            file -= 1;
            index[file] += 1;
            if index[file] < lists[file].len() {
                break;
            }
            index[file] = 0;
        }
    }
    closed.sort_unstable();
    closed.dedup();
    Ok(closed)
}

/// Every choice of which changes of names not synced a crash undoes, as `crash_cut` draws it:
/// any of them, a later one sharing a name with one undone undone too; sorted, each once.
fn undone_sets(moment: &Moment) -> Result<Vec<Vec<bool>>, Refused> {
    let names = &moment.disk.names;
    if names.len() > NAMES_MOST {
        return Err(Refused::TooManyNames { after: moment.after, names: names.len() });
    }
    let mut sets: Vec<Vec<bool>> = Vec::with_capacity_in(1 << names.len(), Global);
    for choice in 0u32..(1u32 << names.len()) {
        let mut undone = Vec::with_capacity_in(names.len(), Global);
        for i in 0..names.len() {
            let forced = (0..i).any(|earlier| undone[earlier] && names[earlier].named.shares(&names[i].named));
            undone.push(forced || (choice >> i) & 1 == 1);
        }
        sets.push(undone);
    }
    sets.sort_unstable();
    sets.dedup();
    Ok(sets)
}

/// How many tears a file has between its first `kept` and first `upto` changes: every length
/// between the two files', each with every vector of landed sectors under it. Saturating.
fn tear_count(file: &SimFile, kept: usize, upto: usize) -> u128 {
    if upto <= kept {
        return 1;
    }
    let (old, new) = (file.len_after(kept) as usize, file.len_after(upto) as usize);
    let (low, high) = (old.min(new), old.max(new));
    let mut total: u128 = 0;
    let mut len = low;
    while len <= high {
        let count = sectors(len);
        // The lengths with as many sectors as `len`: up to the end of its last sector.
        let last = (count * SECTOR).min(high).max(len);
        let lengths = (last - len + 1) as u128;
        let each = if count >= 127 { u128::MAX } else { 1u128 << count };
        total = total.saturating_add(lengths.saturating_mul(each));
        if total == u128::MAX {
            break;
        }
        len = last + 1;
    }
    total
}

/// How many crash points a moment has, made before any disk is built.
fn count(moment: &Moment) -> Result<u128, Refused> {
    let mut total: u128 = 0;
    for (tuples, sets) in groups(moment)? {
        for cut in tuples {
            let mut points = sets.len() as u128;
            for ((_, file), (kept, upto)) in moment.disk.files.iter().zip(&cut) {
                points = points.saturating_mul(tear_count(file, *kept, *upto));
            }
            total = total.saturating_add(points);
        }
    }
    Ok(total)
}

/// Every tear of one file for one closed pair, in order: the lengths ascending, the landed
/// sectors counted up from none (sector 0 the lowest bit).
fn tears_of(file: &SimFile, kept: usize, upto: usize) -> Vec<Option<Tear>> {
    let mut tears = Vec::new_in(Global);
    if upto <= kept {
        tears.push(None);
        return tears;
    }
    let (old, new) = (file.len_after(kept) as usize, file.len_after(upto) as usize);
    for len in old.min(new)..=old.max(new) {
        let count = sectors(len);
        // `count` is small here: `every_cut` counted every tear against its cap first.
        for mask in 0u64..(1u64 << count) {
            let mut landed = Vec::with_capacity_in(count, Global);
            for sector in 0..count {
                landed.push((mask >> sector) & 1 == 1);
            }
            tears.push(Some(Tear { upto, len, landed }));
        }
    }
    tears
}

/// Where `name` is, or would go, in a disk's files sorted by name.
fn find(files: &[Left], name: &FileName) -> Result<usize, usize> {
    files.binary_search_by(|it| it.name.cmp(name))
}

fn put(files: &mut Vec<Left>, left: Left) {
    match find(files, &left.name) {
        Ok(at) => files[at] = left,
        Err(at) => files.insert(at, left),
    }
}

fn take(files: &mut Vec<Left>, name: &FileName) -> Option<Left> {
    find(files, name).ok().map(|at| files.remove(at))
}

/// A file put back by an undo: its durable bytes, as `SimFile::durable_only` leaves it.
fn put_back(name: &FileName, was: &SimFile) -> Left {
    // clone: a refcount and the rebuilt disk's own bytes.
    Left { name: name.share(), link: was.link, bytes: was.durable.clone() }
}

/// The disk one crash point leaves, as `crash_with` makes it: each file's cut, then each
/// change of a name undone, the newest first.
fn left_by(disk: &DiskCopy, files: &[FileCut], undone: &[bool]) -> AfterCrash {
    let mut dirs: Vec<FileName> = Vec::with_capacity_in(disk.dirs.len(), Global);
    // clone: refcounts.
    dirs.extend(disk.dirs.iter().cloned());
    let mut left: Vec<Left> = Vec::with_capacity_in(disk.files.len(), Global);
    for ((name, file), cut) in disk.files.iter().zip(files) {
        let bytes = match &cut.tear {
            None => file.after(cut.whole),
            Some(tear) => mixed(&file.after(cut.whole), &file.after(tear.upto), tear.len, &tear.landed),
        };
        // clone: a refcount.
        left.push(Left { name: name.share(), link: file.link, bytes });
    }
    for (change, undo) in disk.names.iter().zip(undone).rev() {
        if !*undo {
            continue;
        }
        match &change.named {
            Named::Put { file, was } => {
                take(&mut left, file);
                if let Some(was) = was {
                    put(&mut left, put_back(file, was));
                }
            }
            Named::Gone { file, was } => put(&mut left, put_back(file, was)),
            Named::Moved { from, to, was } => {
                if let Some(moved) = take(&mut left, to) {
                    // clone: a refcount.
                    put(&mut left, Left { name: from.share(), ..moved });
                }
                if let Some(was) = was {
                    put(&mut left, put_back(to, was));
                }
            }
            // As `Sim::undo_made_dir`: the directory goes with every file and directory at or
            // under it (every later change under it was undone first, newest first).
            Named::MadeDir { dir } => {
                left.retain(|it| !it.name.is_under(dir));
                dirs.retain(|it| !it.is_under(dir));
            }
            // As `Sim::undo_removed_dir`: back, empty but for the names undone after.
            Named::RemovedDir { dir } => match dirs.binary_search(dir) {
                Ok(_) => unreachable!("{dir}: a directory whose removal is undone is not there: a later make of it was undone first"),
                // clone: a refcount.
                Err(at) => dirs.insert(at, dir.share()),
            },
        }
    }
    AfterCrash { files: left, dirs }
}

/// Every crash point of a moment with the disk it leaves, in the module's order.
fn enumerate(moment: &Moment) -> Result<Vec<(CrashPoint, AfterCrash)>, Refused> {
    let disk = &moment.disk;
    let mut out = Vec::new_in(Global);
    for (tuples, undone) in groups(moment)? {
        for cut in tuples {
            let mut lists = Vec::with_capacity_in(cut.len(), Global);
            for ((_, file), (kept, upto)) in disk.files.iter().zip(&cut) {
                lists.push(tears_of(file, *kept, *upto));
            }
            let mut index = Vec::with_capacity_in(lists.len(), Global);
            index.resize(lists.len(), 0usize);
            loop {
                let mut files = Vec::with_capacity_in(lists.len(), Global);
                for ((((name, file), (kept, _)), list), at) in disk.files.iter().zip(&cut).zip(&lists).zip(&index) {
                    // clone: a refcount and the point's own copy of its tear.
                    files.push(FileCut { name: name.share(), changes: file.ops.len(), whole: *kept, tear: list[*at].clone() });
                }
                for set in &undone {
                    let left = left_by(disk, &files, set);
                    // clone: the point's own copies.
                    let point = CrashPoint { after: moment.after, step: moment.step, files: files.clone(), undone: set.clone() };
                    out.push((point, left));
                }
                // The next combination of tears: the last file's moves fastest; done when all wrap.
                let mut file = lists.len();
                let mut moved = false;
                while file > 0 {
                    file -= 1;
                    index[file] += 1;
                    if index[file] < lists[file].len() {
                        moved = true;
                        break;
                    }
                    index[file] = 0;
                }
                if !moved {
                    break;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_sim::{Broke, Config, check};

    use super::*;
    use crate::sim::Faults;
    use crate::{Address, DirName, run_until_idle};

    fn name(it: &str) -> FileName {
        FileName::new(it).expect("a name")
    }

    /// FNV-1a over a record's length and payload: odd for every input (an odd start times an
    /// odd prime), so a header of zeros never checks.
    fn checksum(len: u32, payload: &[u8]) -> u32 {
        let mut hash: u32 = 0x811c_9dc5;
        for byte in len.to_le_bytes().iter().chain(payload) {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(16_777_619);
        }
        hash
    }

    /// A record: its payload's length, its checksum, then the payload.
    fn record_of(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&checksum(len, payload).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    /// The two payloads: the first record is 504 bytes, so the second's header is in the first
    /// sector and its payload in the second. Never zero.
    fn payloads() -> [Vec<u8>; 2] {
        [(0..496u32).map(|i| (i % 251) as u8 + 1).collect(), (0..40u32).map(|i| (i % 241) as u8 + 3).collect()]
    }

    /// The log's run: append, sync, the name synced, append, sync.
    fn wal_steps() -> Vec<(&'static str, Effect)> {
        let wal = name("wal");
        let [one, two] = payloads();
        vec![
            ("append record 1", Effect::Append { file: wal.clone(), bytes: record_of(&one), tag: Tag(1) }),
            ("sync the log", Effect::Sync { file: wal.clone(), tag: Tag(2) }),
            ("sync the log's name", Effect::SyncDir { dir: DirName::Root, tag: Tag(3) }),
            ("append record 2", Effect::Append { file: wal.clone(), bytes: record_of(&two), tag: Tag(4) }),
            ("sync the log again", Effect::Sync { file: wal, tag: Tag(5) }),
        ]
    }

    /// The records acknowledged after `steps` steps: a record is once its sync and its name's
    /// sync have completed.
    fn acknowledged(steps: usize) -> usize {
        match steps {
            0..=2 => 0,
            3 | 4 => 1,
            _ => 2,
        }
    }

    /// What the log's recovery found wrong.
    #[derive(Debug)]
    enum WalBroken {
        /// A record recovered that was never appended at its place.
        NotAppended { at: usize, len: usize },
        /// Fewer records than were acknowledged.
        Lost { recovered: usize, acknowledged: usize },
        /// The log could not be read.
        Unread(String),
    }

    /// The log's records as a recovery reads them: while a whole header and payload are there
    /// and, unless `trust_length` (the planted bug), the checksum matches.
    fn parse(bytes: &[u8], trust_length: bool) -> Vec<Vec<u8>> {
        let mut records = Vec::new();
        let mut at = 0;
        while at + 8 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
            let sum = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("four bytes"));
            let end = at + 8 + len as usize;
            if end > bytes.len() {
                break;
            }
            let payload = &bytes[at + 8..end];
            if !trust_length && checksum(len, payload) != sum {
                break;
            }
            records.push(payload.to_vec());
            at = end;
        }
        records
    }

    /// The recovery and its invariant: the log read whole; what it recovers is a prefix of
    /// what was appended, and holds every record acknowledged.
    fn recover_wal(sim: &mut Sim, acknowledged: usize, trust_length: bool) -> Result<(), WalBroken> {
        let mut read = None;
        let mut effects = vec![Effect::ReadAll { file: name("wal"), tag: Tag(100) }];
        run_until_idle(sim, &mut effects, |done, _| read = Some(done.done))
            .map_err(|stalled| WalBroken::Unread(format!("{stalled:?}")))?;
        let bytes = match read {
            Some(Done::Read(bytes)) => bytes,
            other => return Err(WalBroken::Unread(format!("{other:?}"))),
        };
        let records = parse(&bytes, trust_length);
        let appended = payloads();
        for (at, record) in records.iter().enumerate() {
            if appended.get(at) != Some(record) {
                return Err(WalBroken::NotAppended { at, len: record.len() });
            }
        }
        if records.len() < acknowledged {
            return Err(WalBroken::Lost { recovered: records.len(), acknowledged });
        }
        Ok(())
    }

    fn sweep_wal(cap: usize, trust_length: bool) -> Result<Swept<WalBroken>, Refused> {
        let mut sim = Sim::new(11, Faults::default());
        every_cut(
            &mut sim,
            Sweep { cap, recovery_seed: 12 },
            |steps| {
                for (what, effect) in wal_steps() {
                    steps.step(what, effect)?;
                }
                Ok(())
            },
            |sim, point| recover_wal(sim, acknowledged(point.after), trust_length),
        )
    }

    /// (a) A tiny write-ahead log with a correct recovery holds at every crash point.
    /// Prints (counted by hand from the module's rules, to verify by the run):
    /// "every cut: 1186 crash points, 0 failures (2207 cuts at 6 moments)".
    #[test]
    fn every_crash_point_of_a_write_ahead_log_recovers() {
        let swept = sweep_wal(1 << 16, false).unwrap_or_else(|refused| panic!("{refused}"));
        eprintln!(
            "every cut: {} crash points, {} failures ({} cuts at {} moments)",
            swept.points,
            swept.failures.len(),
            swept.cuts,
            swept.moments
        );
        if let Some((point, broken)) = swept.failures.first() {
            panic!("the log's recovery failed at {point}: {broken:?}");
        }
        assert_eq!(swept.moments, 6);
        assert!(swept.points > 1000, "{} crash points", swept.points);
    }

    /// (b) The planted bug, a recovery that trusts a record's length without its checksum,
    /// fails at a named crash point; and the random crash (a drawn moment, `Sim::crash`) needs
    /// many seeds to find it where the enumeration needs one run. Prints:
    /// "the planted bug: first at after step 1 ("append record 1"): wal: 0 of 1 changes whole,
    /// then up to change 1 torn to 8 bytes, sectors landed [0]; no change of a name undone"
    /// and "the random crash found it after <k> seeds; the enumeration in 1 run of 1186 crash
    /// points" (k to verify by the run; about fifty by the module's odds).
    #[test]
    fn every_cut_names_where_a_recovery_that_trusts_a_length_fails() {
        let swept = sweep_wal(1 << 16, true).unwrap_or_else(|refused| panic!("{refused}"));
        let Some((point, broken)) = swept.failures.first() else {
            panic!("the planted bug passed every one of {} crash points", swept.points);
        };
        eprintln!("the planted bug: first at {point}: {broken:?}; {} of {} crash points fail", swept.failures.len(), swept.points);
        assert_eq!(point.after, 1, "the first failing point is a torn first record: {point}");
        let steps = wal_steps().len();
        let mut found = None;
        for seed in 0..100_000u64 {
            let mut pick = Rng::new(seed ^ 0x5eed_c0de);
            let crash_after = pick.below(steps as u64 + 1) as usize;
            let mut sim = Sim::new(seed, Faults::default());
            for (_, effect) in wal_steps().into_iter().take(crash_after) {
                let mut effects = vec![effect];
                run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effect ends");
            }
            sim.crash();
            if recover_wal(&mut sim, acknowledged(crash_after), true).is_err() {
                found = Some(seed + 1);
                break;
            }
        }
        let Some(seeds) = found else { panic!("the random crash never found the planted bug in 100000 seeds") };
        eprintln!("the random crash found it after {seeds} seeds; the enumeration in 1 run of {} crash points", swept.points);
    }

    /// (d) Past the cap the answer is the typed refusal with the count, never a sample.
    /// Prints: "refused: too many: 2207 crash points over the cap of 10; every_cut (test.md
    /// S5): the random crash is for a run this long".
    #[test]
    fn past_the_cap_every_cut_refuses_with_the_count() {
        match sweep_wal(10, false) {
            Err(refused @ Refused::TooMany { points, cap: 10 }) => {
                eprintln!("refused: {refused}");
                assert!(points > 10);
            }
            Err(other) => panic!("refused otherwise: {other}"),
            Ok(swept) => panic!("swept {} crash points over a cap of 10", swept.cuts),
        }
    }

    /// The old table and the new: two sectors of the new one, no byte of either zero, so a
    /// tear is told from both.
    fn tables() -> (Vec<u8>, Vec<u8>) {
        (vec![0xEE; 300], (0..600u32).map(|at| (at % 250) as u8 + 1).collect())
    }

    /// A table committed by its name, as blaze-solo commits its table: the old table durable
    /// under its name, the new one written beside it with no sync, a barrier of it, and the
    /// rename over the old with no sync either.
    fn table_steps() -> Vec<(&'static str, Effect)> {
        let (table, part) = (name("st/table"), name("st/table.part"));
        let (old, new) = tables();
        vec![
            ("write the old table", Effect::Write { file: table.clone(), bytes: old, tag: Tag(1) }),
            ("sync the tables' names", Effect::SyncDir { dir: name("st").into(), tag: Tag(2) }),
            ("write the new table beside it", Effect::WriteUnsynced { file: part.clone(), bytes: new, tag: Tag(3) }),
            ("barrier the new table", Effect::Barrier { file: part.clone(), tag: Tag(4) }),
            ("rename it over the old", Effect::RenameUnsynced { from: part, to: table, tag: Tag(5) }),
        ]
    }

    /// What a crash left under the table's name that is neither table.
    #[derive(Debug)]
    enum TableBroken {
        /// No file under the name, once the old table was written.
        Gone,
        /// A file of this many bytes that is neither the old table nor the whole new one.
        Torn { len: usize },
    }

    fn sweep_table(plant: bool) -> Result<Swept<TableBroken>, Refused> {
        let mut sim = Sim::new(13, Faults::default());
        if plant {
            sim.plant(Plant::BarriersNotKept);
        }
        let table = name("st/table");
        every_cut(
            &mut sim,
            Sweep { cap: 1 << 20, recovery_seed: 14 },
            |steps| {
                for (what, effect) in table_steps() {
                    steps.step(what, effect)?;
                }
                Ok(())
            },
            |sim, point| {
                if point.after == 0 {
                    return Ok(());
                }
                if !sim.exists(&table) {
                    return Err(TableBroken::Gone);
                }
                let bytes = sim.contents(&table);
                let (old, new) = tables();
                match bytes == old || bytes == new {
                    true => Ok(()),
                    false => Err(TableBroken::Torn { len: bytes.len() }),
                }
            },
        )
    }

    /// A barrier orders what it covers before a later change of a name: at every crash point
    /// of the commit, the table's name holds the old table or the whole new one. Prints
    /// "every cut of the table's commit: <n> crash points, 0 failures" (n to verify by the
    /// run).
    #[test]
    fn every_crash_point_of_a_rename_after_a_barrier_holds_a_whole_table() {
        let swept = sweep_table(false).unwrap_or_else(|refused| panic!("{refused}"));
        eprintln!("every cut of the table's commit: {} crash points, {} failures", swept.points, swept.failures.len());
        if let Some((point, broken)) = swept.failures.first() {
            panic!("the table's name held a broken table at {point}: {broken:?}");
        }
        assert_eq!(swept.moments, 6);
    }

    /// The planted-bug control: with the barriers keeping nothing, a crash after the rename
    /// leaves a torn table under the name. Prints "the planted bug: first at after step 5
    /// ...: Torn { len: .. }" (the point to verify by the run).
    #[test]
    fn every_cut_finds_a_torn_table_where_the_barriers_keep_nothing() {
        let swept = sweep_table(true).unwrap_or_else(|refused| panic!("{refused}"));
        let Some((point, broken)) = swept.failures.first() else {
            panic!("the planted bug passed every one of {} crash points", swept.points);
        };
        eprintln!("the planted bug: first at {point}: {broken:?}");
        assert_eq!(point.after, 5, "only a kept rename shows the new table's bytes under its name: {point}");
        assert!(matches!(broken, TableBroken::Torn { .. }), "{broken:?}");
    }

    /// The seeds the random side of (c) may take for one moment before it calls a disk of the
    /// enumeration unreachable: its rarest disk is about one in five thousand.
    const RANDOM_MOST: u64 = 200_000;

    /// One step of a drawn journal over two files: a short append (bytes never zero), a sync,
    /// a barrier, or the root's sync.
    fn draw_step(tape: &blaze_sim::Tape, tag: u64) -> (&'static str, Effect) {
        let file = name(["j/a", "j/b"][tape.index(2, "file")]);
        match tape.below(4, "effect") {
            0 => {
                let len = 1 + tape.below(3, "append length") as usize;
                let bytes = (0..len).map(|_| 1 + tape.below(255, "byte") as u8).collect();
                ("append", Effect::Append { file, bytes, tag: Tag(tag) })
            }
            1 => ("sync", Effect::Sync { file, tag: Tag(tag) }),
            2 => ("barrier", Effect::Barrier { file, tag: Tag(tag) }),
            _ => ("sync the root", Effect::SyncDir { dir: DirName::Root, tag: Tag(tag) }),
        }
    }

    /// (c) The enumeration is the random crash's set: for a drawn journal, at every moment,
    /// every disk `Sim::crash` leaves (on a copy of the moment's disk, over seeds) is one the
    /// enumeration has, and every disk the enumeration has some seed leaves. The cap is far
    /// over any drawn journal's count, so the enumeration is whole. Prints nothing of its own;
    /// passes as `blaze_sim::check` does.
    #[test]
    fn the_enumeration_is_the_random_crash_s_set() {
        check(Config::cases(24), |tape| {
            let mut sim = Sim::new(tape.next_u64("the run's seed"), Faults::default());
            let journal: Vec<(&'static str, Effect)> =
                (0..1 + tape.below(4, "steps")).map(|i| draw_step(tape, i + 1)).collect();
            let moments = record(&mut sim, |steps| {
                for (what, effect) in journal {
                    steps.step(what, effect)?;
                }
                Ok(())
            })
            .map_err(|refused| Broke::Harness(format!("{refused}")))?;
            for moment in &moments {
                let mut want: Vec<AfterCrash> = Vec::new();
                for (_, disk) in enumerate(moment).map_err(|refused| Broke::Harness(format!("{refused}")))? {
                    want.push(disk);
                }
                want.sort_unstable();
                want.dedup();
                let mut seen = vec![false; want.len()];
                let mut left = want.len();
                for seed in 0..RANDOM_MOST {
                    if left == 0 {
                        break;
                    }
                    let mut fresh = Sim::new(seed, Faults::default());
                    moment.disk.restore(&mut fresh);
                    fresh.crash();
                    let got = AfterCrash::of(&fresh);
                    match want.binary_search(&got) {
                        Ok(at) => {
                            if !seen[at] {
                                seen[at] = true;
                                left -= 1;
                            }
                        }
                        Err(_) => {
                            return Err(Broke::Oracle(format!(
                                "after step {}: seed {seed}'s crash left {got:?}, which the enumeration lacks",
                                moment.after
                            )));
                        }
                    }
                }
                if let Some(at) = seen.iter().position(|it| !*it) {
                    return Err(Broke::Oracle(format!(
                        "after step {}: {left} disks of the enumeration no seed of {RANDOM_MOST} left, the first {:?}",
                        moment.after, want[at]
                    )));
                }
            }
            Ok(())
        });
    }

    /// The frame a peer sends in one send.
    const FRAME: &[u8] = b"0123456789abcdef";

    /// What one receive of a server end gets of a frame sent whole from outside: a reader that
    /// takes one receive as the frame is wrong exactly when this is short.
    fn one_receive(seed: u64, kinds: &[&'static str]) -> Vec<u8> {
        let mut sim = Sim::new(seed, Faults::default());
        sim.inject(kinds).expect("known kinds");
        let at = Address::Unix(name("frames.sock"));
        let mut effects = vec![Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) }];
        sim.submit(&mut effects);
        let outside = sim.dial(&at).expect("the listen is up");
        sim.peer_send(outside, FRAME);
        let mut out = Vec::new();
        for _ in 0..64 {
            sim.poll(&mut out, Deadline::after(sim.now(), Duration::from_secs(1)));
            for completion in out.drain(..) {
                match completion.done {
                    Done::Accepted { conn } => {
                        effects.push(Effect::Recv { conn, most: 64, into: Vec::new(), tag: Tag(2) })
                    }
                    Done::Read(bytes) if completion.tag == Tag(2) => return bytes,
                    _ => {}
                }
            }
            sim.submit(&mut effects);
        }
        panic!("seed {seed}: no receive ended in 64 polls");
    }

    /// (e) `short-receive`: a reader that takes one receive as a whole frame fails only with
    /// the kind on. Prints: "short-receive: a one-receive frame reader fails on <k> of 64
    /// seeds with the kind, 0 without" (k about half, to verify by the run).
    #[test]
    fn a_short_receive_breaks_a_reader_that_takes_one_receive_as_a_frame() {
        let without = (0..64).filter(|seed| one_receive(*seed, &[]) != FRAME).count();
        let with = (0..64).filter(|seed| one_receive(*seed, &["short-receive"]) != FRAME).count();
        eprintln!("short-receive: a one-receive frame reader fails on {with} of 64 seeds with the kind, {without} without");
        assert_eq!(without, 0);
        assert!(with > 0);
    }

    /// How long a 1 ms timer took to fire, on the virtual clock.
    fn timer_took(seed: u64, kinds: &[&'static str]) -> Duration {
        let mut sim = Sim::new(seed, Faults::default());
        sim.inject(kinds).expect("known kinds");
        let start = sim.now();
        let mut effects = vec![Effect::Timer { after: Duration::from_millis(1), tag: Tag(1) }];
        run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the timer fires");
        sim.now() - start
    }

    /// (e) `clock-jump`: a check that a 1 ms timer fires within a second (a deadline that
    /// forgets the machine may sleep) fails only with the kind on; and a name no kind has is
    /// refused by name. Prints: "clock-jump: a 1 ms timer took over a second on <k> of 64
    /// seeds with the kind, 0 without" (k about a quarter, to verify by the run).
    #[test]
    fn a_clock_jump_breaks_a_deadline_that_forgets_sleep() {
        let second = Duration::from_secs(1);
        let without = (0..64).filter(|seed| timer_took(*seed, &[]) > second).count();
        let with = (0..64).filter(|seed| timer_took(*seed, &["clock-jump"]) > second).count();
        eprintln!("clock-jump: a 1 ms timer took over a second on {with} of 64 seeds with the kind, {without} without");
        assert_eq!(without, 0);
        assert!(with > 0);
        let mut sim = Sim::new(0, Faults::default());
        assert_eq!(sim.inject(&["delivered-twice"]), Err(UnknownKind { name: "delivered-twice" }));
    }
}
