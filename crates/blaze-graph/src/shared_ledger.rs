//! The machine's ledger as shared memory (docs/v2/plan/design/end-state/memory.md, section
//! 4.5): one small file of atomic words, mapped shared by every blaze process of the user, in
//! which each process holds a slot and counts its memory and its work threads' cores. There is
//! no daemon and there are no turns, leases or allowances: a process that wants memory adds it
//! to its own slot and sums every live slot, and one that wants cores does the same with core
//! tokens, so the many processes of one machine (agents' commands in their own checkouts, and
//! their workers) together never admit past the machine's memory budget, nor run more work
//! threads than it has cores (one each past them at most, the floor below).
//!
//! This is the core: it takes the mapping as a `&[AtomicU64]` it does not own, which processes
//! run (and since when) through [`Alive`], and nothing else. The driver (`blaze_core::shared`)
//! makes and maps the file `/tmp/blaze-<uid>/machine.<LAYOUT>` ([`file_name`]) and waits and
//! wakes on its [`EPOCH`] word across processes.
//!
//! # The file
//!
//! [`WORDS`] little-endian `u64` words: a header of [`HEADER`] words, then [`SLOTS`] slots of
//! [`SLOT`] words each (one 64-byte cache line a slot, so two processes' counting never shares a
//! line). All zeros past the header is a ledger with no process.
//!
//! | header word | what                                                                     |
//! |-------------|--------------------------------------------------------------------------|
//! | 0           | [`MAGIC`]                                                                |
//! | 1           | [`LAYOUT`]                                                               |
//! | 2           | the machine's memory budget in bytes (only ever lowered: [`Machine::narrow`]) |
//! | 3           | its cores: the work threads every process together may run               |
//! | 4           | [`EPOCH`]: bumped after every lowering; waiters wait on its low 32 bits  |
//! | 5           | how many threads sleep on the epoch, so a lowering wakes only when one does |
//! | 6, 7        | 0                                                                        |
//!
//! | slot word | what                                                                       |
//! |-----------|----------------------------------------------------------------------------|
//! | 0         | owner: generation (30 bits), pid (32), state (2: free, claiming, live)      |
//! | 1         | the process's start time, in [`Process::start`]'s unit                    |
//! | 2         | bytes reserved                                                             |
//! | 3         | bytes it wants and was refused: what another process's pool may give it    |
//! | 4         | cores held                                                                 |
//! | 5         | cores it wants past those                                                  |
//! | 6         | its beat: when it last said it lives, by its own clock (no decision reads it) |
//! | 7         | 0                                                                          |
//!
//! Only a slot's owner writes its words 1 to 7, and only while the slot is live or it claims it;
//! the owner word moves by compare-and-swap alone. A free slot's other words are whatever its
//! last owner left: nothing reads them, and the next claimer zeroes them before it is live.
//!
//! # Memory: store, then load
//!
//! [`Reserve`] adds the bytes to its own slot, then loads every live slot and sums them. Over
//! the budget, it takes the add back and is refused ([`Reserved::Refused`]). Every word is
//! accessed sequentially consistent, so of two reservers whose adds together pass the budget at
//! least one sees the other's add: each adds before it loads, and in the one order of all the
//! operations one of the two adds comes before the other's load. So at most one passes (both may
//! be refused; the take-back wakes the other, which tries again). The general form: among the
//! reservations granted and still held, the one whose add came last saw every other in its sum,
//! so their bytes sum to at most the budget. `tests.rs` proves it by simulation, interleaving the
//! processes at every atomic operation, and plants the sum-then-add order to see it caught.
//!
//! A refused process sets its want and sleeps on [`EPOCH`] ([`Acquire`], [`Sleep`]): it read the
//! epoch before its add, and sleeps only while the epoch still holds that value, with the kernel
//! comparing the word in the same step as it blocks (a futex, `os_sync_wait_on_address`). Every
//! lowering (a release, a take-back, a slot freed by a leave or a prune, cores given) lowers first,
//! then bumps the epoch, then wakes the sleepers if the count of them is not 0; a sleeper counts
//! itself before it looks at the epoch. Either the lowerer's bump comes before the sleeper's look
//! (the sleeper sees it moved and tries again) or the sleeper's count comes before the lowerer's
//! load of it (the lowerer wakes it). No wake is lost.
//!
//! # Cores
//!
//! [`TakeCores`] is the same shape for core tokens: add what is wanted, sum the live slots' cores,
//! and keep what fits. A process always ends with at least its floor (1: a command can always run
//! one work thread, so nothing waits on a token another process may never give back: rule
//! bounded-progress), so the cores held can pass the machine's cores, by at most the floor of
//! each process that was given its floor beyond what fit: 44 commands on 18 cores run 44 work
//! threads, not 704. That is the right trade: the alternative is a process that cannot start
//! while others hold every core, a wait with no bound behind processes that may be stopped.
//! A process sizes its pool by what it holds, and grows it by taking again after the epoch moves.
//!
//! # Liveness
//!
//! Whoever sums looks at each slot's owner and start: a live slot whose pid no longer runs, or
//! runs with another start time (the pid given to another process), and a claiming slot whose pid
//! no longer runs, is freed with one compare-and-swap of its owner word, which keeps the
//! generation, so a slot freed and claimed again in between is never freed by a stale look. Its
//! bytes and cores stop counting at that swap. A process that dies at any instant leaves a slot
//! the next sum of any other frees, and none of its words are counted again. A live process that
//! stops beating keeps its charge: it may still hold the memory, and freeing its slot would let
//! another process admit past the budget. A sleeper's wait is bounded by the driver's deadline,
//! after which it sums again (which frees the slots of the dead, and wakes the others).
//!
//! The one window that keeps a slot taken with no charge: a claimer that dies between its claim
//! and its live swap, whose pid is then given to another process, keeps its slot claiming until
//! that process exits (its start word may not be written, so its start cannot tell it from the new
//! process). The slot counts nothing; 64 slots are not exhausted by this in practice, and a full
//! ledger is a typed refusal naming the bound, never a wait.
//!
//! Every operation is a state machine ([`Op`]) whose every [`Op::step`] makes at most one access
//! of a shared word, so the simulation can run another process between any two. A driver runs
//! one to its end with [`finish`] (or [`run`], for the one that sleeps).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering::SeqCst},
};

pub use crate::ledger::{Alive, Process};

#[cfg(test)]
mod tests;

/// The file's first word: `blzmachn`, little-endian.
pub const MAGIC: u64 = u64::from_le_bytes(*b"blzmachn");
/// The layout this module reads and writes; it is in the file's name ([`file_name`]), so a blaze
/// of another layout keeps a ledger of its own, and in the file, which a driver checks.
pub const LAYOUT: u64 = 1;
/// The most blaze processes of one user that share one machine's ledger. A process past them is
/// refused, naming the bound ([`Why::Full`]): 64 is past what one machine runs at once (44
/// processes were seen on 2026-11-13), and a slot is one cache line, so the whole file is a page.
pub const SLOTS: usize = 64;
/// Words of the header.
pub const HEADER: usize = 8;
/// Words of a slot.
pub const SLOT: usize = 8;
/// Words of the whole file.
pub const WORDS: usize = HEADER + SLOTS * SLOT;
/// The header's words a file must hold exactly to be this ledger: its magic and layout.
pub const IDENTITY: usize = 2;
/// The word waiters wait on (its low 32 bits) and every lowering bumps.
pub const EPOCH: usize = 4;

const AT_MAGIC: usize = 0;
const AT_LAYOUT: usize = 1;
const AT_BUDGET: usize = 2;
const AT_CORES: usize = 3;
const AT_SLEEPERS: usize = 5;

const OWNER: usize = 0;
const START: usize = 1;
const RESERVED: usize = 2;
const WANT: usize = 3;
const CORES: usize = 4;
const CORES_WANT: usize = 5;
const BEAT: usize = 6;

/// The words a claimer zeroes before its slot is live.
const ZEROED: [usize; 5] = [RESERVED, WANT, CORES, CORES_WANT, BEAT];

/// The most steps of an operation that does not sleep: two passes of every slot at a few
/// accesses each, and its own handful. An operation past it is a bug, named ([`Why::Stuck`]).
pub const MOST_STEPS: usize = 4 * SLOTS * SLOT + 64;
/// The most rounds an [`Acquire`] tries again with no sleep between, the epoch having moved
/// each time; past it the machine is churning and the acquire ends naming it ([`Why::Churn`]).
pub const CHURN: u32 = 1024;

/// The file's name in the user's directory (`/tmp/blaze-<uid>/`).
pub fn file_name() -> String {
    // alloc: a file name, once a process opens the ledger
    format!("machine.{LAYOUT}")
}

const FREE: u64 = 0;
const CLAIMING: u64 = 1;
const LIVE: u64 = 2;
const GEN_SHIFT: u32 = 34;
const GEN_MASK: u64 = (1 << 30) - 1;

const fn owner(generation: u64, pid: u32, state: u64) -> u64 {
    ((generation & GEN_MASK) << GEN_SHIFT) | ((pid as u64) << 2) | state
}

const fn state_of(owner: u64) -> u64 {
    owner & 3
}

const fn pid_of(owner: u64) -> u32 {
    ((owner >> 2) & 0xffff_ffff) as u32
}

const fn generation_of(owner: u64) -> u64 {
    owner >> GEN_SHIFT
}

/// The owner word of the slot `owner` held, freed: its generation kept, so a look at the old
/// owner fails its swap.
const fn freed(owner: u64) -> u64 {
    self::owner(generation_of(owner), 0, FREE)
}

const fn at(slot: usize, field: usize) -> usize {
    HEADER + slot * SLOT + field
}

/// A planted bug the DST must catch (`testing` skill): each is caught by an oracle in one of the
/// seeded plans its control searches.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// [`Reserve`] sums the slots, then adds its own bytes if the sum and they fit: two reservers
    /// that each sum before the other adds both pass.
    SumThenAdd,
    /// [`Release`] bumps the epoch and wakes before it lowers its slot: a sleeper woken tries,
    /// is refused by what is not yet lowered, and sleeps on through the lowering.
    WakeBeforeLower,
    /// A sum counts the slot of a dead process it frees.
    PrunedStillCounted,
    /// A slot's process is taken as alive when its pid runs, whatever its start time: a dead
    /// process's charge kept by the process that was given its pid.
    PidWithoutStart,
}

/// Whether `$machine` carries the plant `$plant`: false outside tests.
macro_rules! planted {
    ($machine:expr, $plant:ident) => {{
        #[cfg(test)]
        let planted = $machine.plant == Plant::$plant;
        #[cfg(not(test))]
        let planted = false;
        planted
    }};
}

/// The ledger's words, checked: the mapping a driver made, and the path every error names.
pub struct Machine<'m> {
    path: &'m Path,
    words: &'m [AtomicU64],
    #[cfg(test)]
    pub(crate) plant: Plant,
}

/// A process's place in the ledger: which slot, and the owner word it holds there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    index: usize,
    owner: u64,
    process: Process,
}

impl Slot {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn process(&self) -> Process {
        self.process
    }
}

impl<'m> Machine<'m> {
    /// The header a new file is stamped with: the first process's budget and cores.
    pub fn header(budget: u64, cores: u64) -> [u64; HEADER] {
        [MAGIC, LAYOUT, budget, cores, 0, 0, 0, 0]
    }

    /// The ledger in `words`, the file `path` mapped: refused, naming the file, when it is not
    /// [`WORDS`] long, or its header is not this layout's.
    pub fn new(path: &'m Path, words: &'m [AtomicU64]) -> Result<Machine<'m>, MachineError> {
        let machine = Machine {
            path,
            words,
            #[cfg(test)]
            plant: Plant::None,
        };
        if words.len() != WORDS {
            return Err(machine.error(Why::Length { words: words.len() }));
        }
        let magic = machine.load(AT_MAGIC);
        if magic != MAGIC {
            return Err(machine.error(Why::Magic { found: magic }));
        }
        let layout = machine.load(AT_LAYOUT);
        if layout != LAYOUT {
            return Err(machine.error(Why::Layout { found: layout }));
        }
        for word in [AT_BUDGET, AT_CORES] {
            if machine.load(word) == 0 {
                return Err(machine.error(Why::Header { word, found: 0 }));
            }
        }
        for word in 6..HEADER {
            let found = machine.load(word);
            if found != 0 {
                return Err(machine.error(Why::Header { word, found }));
            }
        }
        Ok(machine)
    }

    pub fn path(&self) -> &'m Path {
        self.path
    }

    /// The machine's memory budget now.
    pub fn budget(&self) -> u64 {
        self.load(AT_BUDGET)
    }

    /// The machine's cores.
    pub fn cores(&self) -> u64 {
        self.load(AT_CORES)
    }

    /// The epoch now: a driver's [`Sleep`] waits while it holds this.
    pub fn epoch(&self) -> u64 {
        self.load(EPOCH)
    }

    /// Lowers the machine budget to `budget` if that is less (a process told a smaller one,
    /// `BLAZE_MACHINE_MEMORY_GB`): the least any process brought holds for all, as in today's
    /// ledger. Never raised: a budget a process was told is never passed by another's. The budget
    /// now. Lowering it frees nothing, so no one is woken.
    pub fn narrow(&self, budget: u64) -> u64 {
        let budget = budget.max(1);
        self.words[AT_BUDGET].fetch_min(budget, SeqCst).min(budget)
    }

    /// Says `slot`'s process lives, at `at_ns` by its own clock: for whoever reads the ledger.
    /// No decision reads it (a live process's charge stays, beating or not).
    pub fn beat(&self, slot: Slot, at_ns: u64) -> Result<(), MachineError> {
        self.own(slot)?;
        self.store(at(slot.index, BEAT), at_ns);
        Ok(())
    }

    /// Takes back what `slot` wants of memory and cores: a driver whose acquire ended without a
    /// grant (its deadline) says it no longer waits.
    pub fn withdraw(&self, slot: Slot) -> Result<(), MachineError> {
        self.own(slot)?;
        self.store(at(slot.index, WANT), 0);
        self.store(at(slot.index, CORES_WANT), 0);
        Ok(())
    }

    fn load(&self, word: usize) -> u64 {
        self.words[word].load(SeqCst)
    }

    fn store(&self, word: usize, value: u64) {
        self.words[word].store(value, SeqCst);
    }

    fn add(&self, word: usize, value: u64) -> u64 {
        self.words[word].fetch_add(value, SeqCst)
    }

    fn sub(&self, word: usize, value: u64) -> u64 {
        self.words[word].fetch_sub(value, SeqCst)
    }

    fn swap(&self, word: usize, from: u64, to: u64) -> bool {
        self.words[word].compare_exchange(from, to, SeqCst, SeqCst).is_ok()
    }

    /// Whether a live slot's process runs: its pid runs, and started when the slot says.
    fn lives(&self, started: Option<u64>, start: u64) -> bool {
        if planted!(self, PidWithoutStart) {
            return started.is_some();
        }
        started == Some(start)
    }

    /// `slot` is still its process's: a slot freed under a live process is a bug, named.
    fn own(&self, slot: Slot) -> Result<(), MachineError> {
        if self.load(at(slot.index, OWNER)) == slot.owner {
            return Ok(());
        }
        let Process { pid, start } = slot.process;
        Err(self.error(Why::Lost { slot: slot.index, pid, start }))
    }

    fn error(&self, why: Why) -> MachineError {
        // alloc: the path an error names, once an error
        MachineError { path: self.path.to_path_buf(), why }
    }
}

/// What one step of an operation asks of its driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step<T> {
    /// Step again.
    Next,
    /// Wake every thread sleeping on [`EPOCH`] (a futex wake of all, an
    /// `os_sync_wake_by_address_all`), then step again.
    Wake,
    /// Sleep while [`EPOCH`]'s low 32 bits hold `expected`, until a wake or the driver's deadline
    /// for one wait, then step again either way.
    Block { expected: u32 },
    Done(T),
}

impl<T> Step<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Step<U> {
        match self {
            Step::Next => Step::Next,
            Step::Wake => Step::Wake,
            Step::Block { expected } => Step::Block { expected },
            Step::Done(out) => Step::Done(f(out)),
        }
    }
}

/// One operation on the ledger, stepped: each step makes at most one access of a shared word.
pub trait Op {
    type Out;
    /// What it is, for an error.
    const NAME: &'static str;

    fn step<A: Alive>(
        &mut self,
        machine: &Machine<'_>,
        alive: &A,
    ) -> Result<Step<Self::Out>, MachineError>;
}

/// Runs `op` to its end, `wake` doing each wake it asks for. For the operations that never
/// sleep: one that asks to is a bug, named, as is one past [`MOST_STEPS`].
pub fn finish<O: Op, A: Alive>(
    op: &mut O,
    machine: &Machine<'_>,
    alive: &A,
    mut wake: impl FnMut(),
) -> Result<O::Out, MachineError> {
    for _ in 0..MOST_STEPS {
        match op.step(machine, alive)? {
            Step::Next => {}
            Step::Wake => wake(),
            Step::Block { .. } => return Err(machine.error(Why::Blocks { op: O::NAME })),
            Step::Done(out) => return Ok(out),
        }
    }
    Err(machine.error(Why::Stuck { op: O::NAME, steps: MOST_STEPS }))
}

/// What a driver does for an operation that may sleep: its wakes and its waits.
pub trait Host {
    type Error;
    /// Wakes every thread sleeping on [`EPOCH`].
    fn wake(&mut self);
    /// Sleeps while [`EPOCH`]'s low 32 bits hold `expected`, until a wake or the end of one wait
    /// (the driver's recheck, after which the operation sums again and frees the slots of the
    /// dead); an error ends the operation (the driver's whole deadline passed).
    fn block(&mut self, expected: u32) -> Result<(), Self::Error>;
}

/// How a [`run`] failed: the ledger's error, or the host's.
#[derive(Debug)]
pub enum Ran<E> {
    Machine(MachineError),
    Host(E),
}

/// Runs `op` to its end, sleeping through `host` where it asks. The steps between two sleeps are
/// bounded by [`MOST_STEPS`]; the sleeps by the host's deadline.
pub fn run<O: Op, A: Alive, H: Host>(
    op: &mut O,
    machine: &Machine<'_>,
    alive: &A,
    host: &mut H,
) -> Result<O::Out, Ran<H::Error>> {
    let mut steps = 0;
    loop {
        steps += 1;
        if steps > MOST_STEPS {
            return Err(Ran::Machine(machine.error(Why::Stuck { op: O::NAME, steps: MOST_STEPS })));
        }
        match op.step(machine, alive).map_err(Ran::Machine)? {
            Step::Next => {}
            Step::Wake => host.wake(),
            Step::Block { expected } => {
                host.block(expected).map_err(Ran::Host)?;
                steps = 0;
            }
            Step::Done(out) => return Ok(out),
        }
    }
}

// --- The sum ---------------------------------------------------------------------------------

/// A pass over every slot that sums `fields` of the live ones and frees the slots of the dead.
#[derive(Clone, Copy, Debug)]
struct Scan {
    fields: &'static [usize],
    slot: usize,
    at: ScanAt,
    sums: [u64; 4],
    live: u32,
    pruned: u32,
}

#[derive(Clone, Copy, Debug)]
enum ScanAt {
    Owner,
    Start { owner: u64 },
    Field { owner: u64, k: usize, prune: bool },
    Prune { owner: u64 },
    Sleepers,
    End,
}

enum Scanned {
    Next,
    Wake,
    Done,
}

impl Scan {
    fn new(fields: &'static [usize]) -> Scan {
        debug_assert!(fields.len() <= 4);
        Scan { fields, slot: 0, at: ScanAt::Owner, sums: [0; 4], live: 0, pruned: 0 }
    }

    fn step<A: Alive>(&mut self, m: &Machine<'_>, alive: &A) -> Result<Scanned, MachineError> {
        match self.at {
            ScanAt::Owner => {
                if self.slot == SLOTS {
                    if self.pruned == 0 {
                        self.at = ScanAt::End;
                        return Ok(Scanned::Done);
                    }
                    // Slots were freed: their charge no longer counts, so a sleeper may fit.
                    m.add(EPOCH, 1);
                    self.at = ScanAt::Sleepers;
                    return Ok(Scanned::Next);
                }
                let owner = m.load(at(self.slot, OWNER));
                match state_of(owner) {
                    LIVE | CLAIMING => self.at = ScanAt::Start { owner },
                    FREE => self.slot += 1,
                    _ => return Err(m.error(Why::Owner { slot: self.slot, word: owner })),
                }
                Ok(Scanned::Next)
            }
            ScanAt::Start { owner } => {
                let start = m.load(at(self.slot, START));
                let started = alive.started(pid_of(owner));
                let live = state_of(owner) == LIVE;
                // A claimer's start may not be written yet: it is dead only once its pid is.
                let dead = if live { !m.lives(started, start) } else { started.is_none() };
                self.at = match (dead, live) {
                    (true, _) if planted!(m, PrunedStillCounted) => {
                        ScanAt::Field { owner, k: 0, prune: true }
                    }
                    (true, _) => ScanAt::Prune { owner },
                    (false, true) => ScanAt::Field { owner, k: 0, prune: false },
                    (false, false) => {
                        self.slot += 1;
                        ScanAt::Owner
                    }
                };
                Ok(Scanned::Next)
            }
            ScanAt::Field { owner, k, prune } => {
                let value = m.load(at(self.slot, self.fields[k]));
                self.sums[k] = self.sums[k].saturating_add(value);
                self.at = if k + 1 < self.fields.len() {
                    ScanAt::Field { owner, k: k + 1, prune }
                } else if prune {
                    ScanAt::Prune { owner }
                } else {
                    self.live += 1;
                    self.slot += 1;
                    ScanAt::Owner
                };
                Ok(Scanned::Next)
            }
            ScanAt::Prune { owner } => {
                // Fails when another freed it first, or it was freed and claimed again since the
                // look: the generation moved.
                if m.swap(at(self.slot, OWNER), owner, freed(owner)) {
                    self.pruned += 1;
                }
                self.slot += 1;
                self.at = ScanAt::Owner;
                Ok(Scanned::Next)
            }
            ScanAt::Sleepers => {
                self.at = ScanAt::End;
                Ok(if m.load(AT_SLEEPERS) > 0 { Scanned::Wake } else { Scanned::Next })
            }
            ScanAt::End => Ok(Scanned::Done),
        }
    }
}

// --- Joining and leaving ---------------------------------------------------------------------

/// Takes a free slot for `me`: a first pass for a free one; when there is none, a sum that frees
/// the slots of the dead, and a second pass. None then: [`Why::Full`].
pub struct Join {
    me: Process,
    at: JoinAt,
    slot: usize,
    second: bool,
    scan: Scan,
    claimed: u64,
}

#[derive(Clone, Copy, Debug)]
enum JoinAt {
    Look,
    Claim { free: u64 },
    Start,
    Zero { k: usize },
    Live,
    Scan,
}

impl Join {
    pub fn new(me: Process) -> Join {
        Join {
            me,
            at: JoinAt::Look,
            slot: 0,
            second: false,
            scan: Scan::new(&[RESERVED]),
            claimed: 0,
        }
    }
}

impl Op for Join {
    type Out = Slot;
    const NAME: &'static str = "a join of the machine's ledger";

    fn step<A: Alive>(&mut self, m: &Machine<'_>, alive: &A) -> Result<Step<Slot>, MachineError> {
        if self.me.pid == 0 {
            return Err(m.error(Why::NoPid));
        }
        match self.at {
            JoinAt::Look => {
                if self.slot == SLOTS {
                    if self.second {
                        return Err(m.error(Why::Full));
                    }
                    self.second = true;
                    self.slot = 0;
                    self.at = JoinAt::Scan;
                    return Ok(Step::Next);
                }
                let owner = m.load(at(self.slot, OWNER));
                if state_of(owner) == FREE {
                    self.at = JoinAt::Claim { free: owner };
                } else {
                    self.slot += 1;
                }
            }
            JoinAt::Claim { free } => {
                let claimed = owner(generation_of(free) + 1, self.me.pid, CLAIMING);
                if m.swap(at(self.slot, OWNER), free, claimed) {
                    self.claimed = claimed;
                    self.at = JoinAt::Start;
                } else {
                    self.slot += 1;
                    self.at = JoinAt::Look;
                }
            }
            JoinAt::Start => {
                m.store(at(self.slot, START), self.me.start);
                self.at = JoinAt::Zero { k: 0 };
            }
            JoinAt::Zero { k } => {
                m.store(at(self.slot, ZEROED[k]), 0);
                self.at = if k + 1 < ZEROED.len() { JoinAt::Zero { k: k + 1 } } else { JoinAt::Live };
            }
            JoinAt::Live => {
                let live = owner(generation_of(self.claimed), self.me.pid, LIVE);
                // A claimer is freed only once its pid no longer runs: it runs, so the swap holds.
                if !m.swap(at(self.slot, OWNER), self.claimed, live) {
                    let Process { pid, start } = self.me;
                    return Err(m.error(Why::Lost { slot: self.slot, pid, start }));
                }
                return Ok(Step::Done(Slot { index: self.slot, owner: live, process: self.me }));
            }
            JoinAt::Scan => match self.scan.step(m, alive)? {
                Scanned::Next => {}
                Scanned::Wake => return Ok(Step::Wake),
                Scanned::Done => self.at = JoinAt::Look,
            },
        }
        Ok(Step::Next)
    }
}

/// Frees `slot`: a clean exit. Its charge stops counting at the swap; sleepers are woken.
pub struct Leave {
    slot: Slot,
    at: Lowering,
}

/// The tail every lowering shares: bump the epoch, then wake if anyone sleeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lowering {
    Own,
    Check,
    Lower,
    Bump,
    Sleepers,
    End,
}

impl Leave {
    pub fn new(slot: Slot) -> Leave {
        Leave { slot, at: Lowering::Lower }
    }
}

impl Op for Leave {
    type Out = ();
    const NAME: &'static str = "a leave of the machine's ledger";

    fn step<A: Alive>(&mut self, m: &Machine<'_>, _: &A) -> Result<Step<()>, MachineError> {
        match self.at {
            Lowering::Lower => {
                if !m.swap(at(self.slot.index, OWNER), self.slot.owner, freed(self.slot.owner)) {
                    let Process { pid, start } = self.slot.process;
                    return Err(m.error(Why::Lost { slot: self.slot.index, pid, start }));
                }
                self.at = Lowering::Bump;
            }
            Lowering::Bump => {
                m.add(EPOCH, 1);
                self.at = Lowering::Sleepers;
            }
            Lowering::Sleepers => {
                self.at = Lowering::End;
                if m.load(AT_SLEEPERS) > 0 {
                    return Ok(Step::Wake);
                }
            }
            Lowering::End => return Ok(Step::Done(())),
            Lowering::Own | Lowering::Check => unreachable!("a leave starts at its swap"),
        }
        Ok(Step::Next)
    }
}

// --- Memory ----------------------------------------------------------------------------------

/// What a [`Reserve`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reserved {
    /// The bytes are the process's. `counted`: the live slots' bytes the sum saw, these in it.
    Granted { counted: u64, budget: u64 },
    /// Over the budget by `over_by`: the add was taken back and the want set. `epoch`: what to
    /// sleep on ([`Sleep`]): the epoch read before the add, or one past it when this reserve's
    /// own take-back was the only bump since.
    Refused { over_by: u64, counted: u64, budget: u64, epoch: u64 },
}

/// One try at `bytes` more for `slot`: add, sum, and keep or take back.
pub struct Reserve {
    slot: Slot,
    bytes: u64,
    at: ReserveAt,
    epoch: u64,
    scan: Scan,
    out: Option<Reserved>,
}

#[derive(Clone, Copy, Debug)]
enum ReserveAt {
    Own,
    Epoch,
    Add,
    Scan,
    Budget,
    TakeBack,
    Bump,
    Sleepers,
    Want,
}

impl Reserve {
    pub fn new(slot: Slot, bytes: u64) -> Reserve {
        Reserve {
            slot,
            bytes,
            at: ReserveAt::Own,
            epoch: 0,
            scan: Scan::new(&[RESERVED]),
            out: None,
        }
    }
}

impl Op for Reserve {
    type Out = Reserved;
    const NAME: &'static str = "a reserve of the machine's memory";

    fn step<A: Alive>(
        &mut self,
        m: &Machine<'_>,
        alive: &A,
    ) -> Result<Step<Reserved>, MachineError> {
        let slot = self.slot.index;
        match self.at {
            ReserveAt::Own => {
                m.own(self.slot)?;
                self.at = ReserveAt::Epoch;
            }
            ReserveAt::Epoch => {
                // Before the add: a lowering after this read is one the sleep sees.
                self.epoch = m.load(EPOCH);
                self.at =
                    if planted!(m, SumThenAdd) { ReserveAt::Scan } else { ReserveAt::Add };
            }
            ReserveAt::Add => {
                m.add(at(slot, RESERVED), self.bytes);
                self.at =
                    if planted!(m, SumThenAdd) { ReserveAt::Want } else { ReserveAt::Scan };
            }
            ReserveAt::Scan => match self.scan.step(m, alive)? {
                Scanned::Next => {}
                Scanned::Wake => return Ok(Step::Wake),
                Scanned::Done => self.at = ReserveAt::Budget,
            },
            ReserveAt::Budget => {
                let budget = m.load(AT_BUDGET);
                let counted = self.scan.sums[0];
                let total = if planted!(m, SumThenAdd) {
                    counted.saturating_add(self.bytes)
                } else {
                    counted
                };
                if total <= budget {
                    self.out = Some(Reserved::Granted { counted, budget });
                    self.at =
                        if planted!(m, SumThenAdd) { ReserveAt::Add } else { ReserveAt::Want };
                } else {
                    let over_by = total - budget;
                    let epoch = self.epoch;
                    self.out = Some(Reserved::Refused { over_by, counted, budget, epoch });
                    self.at =
                        if planted!(m, SumThenAdd) { ReserveAt::Want } else { ReserveAt::TakeBack };
                }
            }
            ReserveAt::TakeBack => {
                m.sub(at(slot, RESERVED), self.bytes);
                self.at = ReserveAt::Bump;
            }
            ReserveAt::Bump => {
                // The take-back is a lowering: another reserver refused for this add may now fit.
                let was = m.add(EPOCH, 1);
                // Only this bump since the read: sleeping on the epoch it made loses no other
                // lowering, and does not wake this reserve at once for its own.
                if was == self.epoch
                    && let Some(Reserved::Refused { epoch, .. }) = &mut self.out
                {
                    *epoch = was.wrapping_add(1);
                }
                self.at = ReserveAt::Sleepers;
            }
            ReserveAt::Sleepers => {
                self.at = ReserveAt::Want;
                if m.load(AT_SLEEPERS) > 0 {
                    return Ok(Step::Wake);
                }
            }
            ReserveAt::Want => {
                let out = self.out.expect("a reserve decides before it sets its want");
                let want = match out {
                    Reserved::Granted { .. } => 0,
                    Reserved::Refused { .. } => self.bytes,
                };
                m.store(at(slot, WANT), want);
                return Ok(Step::Done(out));
            }
        }
        Ok(Step::Next)
    }
}

/// Gives `bytes` of `slot`'s reservation back: lowers the slot, then bumps the epoch and wakes.
pub struct Release {
    slot: Slot,
    bytes: u64,
    charge: Charge,
    at: Lowering,
    holds: u64,
}

impl Release {
    pub fn new(slot: Slot, bytes: u64) -> Release {
        Release { slot, bytes, charge: Charge::Bytes, at: Lowering::Own, holds: 0 }
    }
}

impl Op for Release {
    /// What the slot holds after.
    type Out = u64;
    const NAME: &'static str = "a release of the machine's memory";

    fn step<A: Alive>(&mut self, m: &Machine<'_>, _: &A) -> Result<Step<u64>, MachineError> {
        lower(m, self.slot, self.bytes, self.charge, &mut self.at, &mut self.holds)
    }
}

/// A lowering of `slot`'s `charge` by `by`, stepped: own, check, lower, bump, wake.
fn lower(
    m: &Machine<'_>,
    slot: Slot,
    by: u64,
    charge: Charge,
    state: &mut Lowering,
    holds: &mut u64,
) -> Result<Step<u64>, MachineError> {
    let word = at(slot.index, charge.field());
    // The plant's order: bump and wake, then lower.
    let early = charge == Charge::Bytes && planted!(m, WakeBeforeLower);
    match *state {
        Lowering::Own => {
            m.own(slot)?;
            *state = Lowering::Check;
        }
        Lowering::Check => {
            // Only the owner writes its slot's counts while it is live: what it loads stays.
            *holds = m.load(word);
            if *holds < by {
                return Err(m.error(Why::Over {
                    slot: slot.index,
                    charge,
                    holds: *holds,
                    gives: by,
                }));
            }
            *state = if early { Lowering::Bump } else { Lowering::Lower };
        }
        Lowering::Lower => {
            m.sub(word, by);
            if early {
                return Ok(Step::Done(*holds - by));
            }
            *state = Lowering::Bump;
        }
        Lowering::Bump => {
            m.add(EPOCH, 1);
            *state = Lowering::Sleepers;
        }
        Lowering::Sleepers => {
            *state = if early { Lowering::Lower } else { Lowering::End };
            if m.load(AT_SLEEPERS) > 0 {
                return Ok(Step::Wake);
            }
        }
        Lowering::End => return Ok(Step::Done(*holds - by)),
    }
    Ok(Step::Next)
}

/// What an [`Acquire`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Acquired {
    /// What the granting sum saw, these bytes in it.
    pub counted: u64,
    pub budget: u64,
    /// How many times it was refused first.
    pub refusals: u32,
}

/// `bytes` more for `slot`, sleeping on the epoch between refusals until they fit. A refusal of
/// more than the whole budget is an error, naming both: no release makes it fit.
pub struct Acquire {
    slot: Slot,
    bytes: u64,
    at: AcquireAt,
    refusals: u32,
    awake: u32,
    last: Option<Reserved>,
}

enum AcquireAt {
    Reserve(Reserve),
    Sleep(Sleep),
}

impl Acquire {
    pub fn new(slot: Slot, bytes: u64) -> Acquire {
        Acquire {
            slot,
            bytes,
            at: AcquireAt::Reserve(Reserve::new(slot, bytes)),
            refusals: 0,
            awake: 0,
            last: None,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The last refusal, for a driver whose deadline passed to name what held the memory.
    pub fn refused(&self) -> Option<Reserved> {
        self.last
    }
}

impl Op for Acquire {
    type Out = Acquired;
    const NAME: &'static str = "an acquire of the machine's memory";

    fn step<A: Alive>(
        &mut self,
        m: &Machine<'_>,
        alive: &A,
    ) -> Result<Step<Acquired>, MachineError> {
        match &mut self.at {
            AcquireAt::Reserve(reserve) => match reserve.step(m, alive)? {
                Step::Done(Reserved::Granted { counted, budget }) => Ok(Step::Done(Acquired {
                    counted,
                    budget,
                    refusals: self.refusals,
                })),
                Step::Done(refused @ Reserved::Refused { budget, epoch, .. }) => {
                    if self.bytes > budget {
                        return Err(m.error(Why::TooBig { bytes: self.bytes, budget }));
                    }
                    self.refusals += 1;
                    self.last = Some(refused);
                    self.at = AcquireAt::Sleep(Sleep::new(epoch as u32));
                    Ok(Step::Next)
                }
                other => Ok(other.map(|_| unreachable!("matched above"))),
            },
            AcquireAt::Sleep(sleep) => match sleep.step(m, alive)? {
                Step::Done(slept) => {
                    if slept {
                        self.awake = 0;
                    } else {
                        self.awake += 1;
                        if self.awake > CHURN {
                            return Err(m.error(Why::Churn { rounds: CHURN }));
                        }
                    }
                    self.at = AcquireAt::Reserve(Reserve::new(self.slot, self.bytes));
                    Ok(Step::Next)
                }
                other => Ok(other.map(|_| unreachable!("matched above"))),
            },
        }
    }
}

/// A sleep on the epoch while it holds `expected`: counted among the sleepers first, so a
/// lowering that bumps after the look wakes it.
pub struct Sleep {
    expected: u32,
    at: SleepAt,
}

#[derive(Clone, Copy, Debug)]
enum SleepAt {
    Count,
    Look,
    Blocked,
    Uncount,
}

impl Sleep {
    pub fn new(expected: u32) -> Sleep {
        Sleep { expected, at: SleepAt::Count }
    }
}

impl Op for Sleep {
    /// Whether it blocked: false when the epoch had moved already.
    type Out = bool;
    const NAME: &'static str = "a sleep on the machine's ledger";

    fn step<A: Alive>(&mut self, m: &Machine<'_>, _: &A) -> Result<Step<bool>, MachineError> {
        match self.at {
            SleepAt::Count => {
                m.add(AT_SLEEPERS, 1);
                self.at = SleepAt::Look;
                Ok(Step::Next)
            }
            SleepAt::Look => {
                if m.load(EPOCH) as u32 != self.expected {
                    self.at = SleepAt::Uncount;
                    return Ok(Step::Next);
                }
                self.at = SleepAt::Blocked;
                Ok(Step::Block { expected: self.expected })
            }
            SleepAt::Blocked => {
                m.sub(AT_SLEEPERS, 1);
                Ok(Step::Done(true))
            }
            SleepAt::Uncount => {
                m.sub(AT_SLEEPERS, 1);
                Ok(Step::Done(false))
            }
        }
    }
}

// --- Cores -----------------------------------------------------------------------------------

/// What a [`TakeCores`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cores {
    /// The cores given now.
    pub granted: u64,
    /// All the process holds now: what its pool runs.
    pub held: u64,
    /// By how much the sum passed the machine's cores, this take's whole want in it.
    pub over: u64,
    /// What the sum saw of every live slot's cores, this take's whole want in it.
    pub counted: u64,
    pub cores: u64,
    /// What to sleep on to take again once some are given back.
    pub epoch: u64,
}

/// `want` more cores for `slot`, ending with at least `floor` held: add, sum, keep what fits (or
/// the floor) and give the rest back at once.
pub struct TakeCores {
    slot: Slot,
    want: u64,
    floor: u64,
    at: TakeAt,
    epoch: u64,
    old: u64,
    scan: Scan,
    out: Option<Cores>,
}

#[derive(Clone, Copy, Debug)]
enum TakeAt {
    Own,
    Epoch,
    Add,
    Scan,
    Decide,
    Back,
    Bump,
    Sleepers,
    Want,
}

impl TakeCores {
    pub fn new(slot: Slot, want: u64, floor: u64) -> TakeCores {
        TakeCores {
            slot,
            want,
            floor,
            at: TakeAt::Own,
            epoch: 0,
            old: 0,
            scan: Scan::new(&[CORES]),
            out: None,
        }
    }

    pub fn want(&self) -> u64 {
        self.want
    }
}

impl Op for TakeCores {
    type Out = Cores;
    const NAME: &'static str = "a take of the machine's cores";

    fn step<A: Alive>(&mut self, m: &Machine<'_>, alive: &A) -> Result<Step<Cores>, MachineError> {
        let slot = self.slot.index;
        match self.at {
            TakeAt::Own => {
                m.own(self.slot)?;
                self.at = TakeAt::Epoch;
            }
            TakeAt::Epoch => {
                self.epoch = m.load(EPOCH);
                self.at = TakeAt::Add;
            }
            TakeAt::Add => {
                self.old = m.add(at(slot, CORES), self.want);
                self.at = TakeAt::Scan;
            }
            TakeAt::Scan => match self.scan.step(m, alive)? {
                Scanned::Next => {}
                Scanned::Wake => return Ok(Step::Wake),
                Scanned::Done => self.at = TakeAt::Decide,
            },
            TakeAt::Decide => {
                let cores = m.load(AT_CORES);
                let counted = self.scan.sums[0];
                let over = counted.saturating_sub(cores);
                let fit = self.want.saturating_sub(over);
                let granted = fit.max(self.floor.saturating_sub(self.old)).min(self.want);
                self.out = Some(Cores {
                    granted,
                    held: self.old + granted,
                    over,
                    counted,
                    cores,
                    epoch: self.epoch,
                });
                self.at = if granted < self.want { TakeAt::Back } else { TakeAt::Want };
            }
            TakeAt::Back => {
                let out = self.out.expect("a take decides before it gives back");
                m.sub(at(slot, CORES), self.want - out.granted);
                self.at = TakeAt::Bump;
            }
            TakeAt::Bump => {
                let was = m.add(EPOCH, 1);
                if was == self.epoch
                    && let Some(out) = &mut self.out
                {
                    out.epoch = was.wrapping_add(1);
                }
                self.at = TakeAt::Sleepers;
            }
            TakeAt::Sleepers => {
                self.at = TakeAt::Want;
                if m.load(AT_SLEEPERS) > 0 {
                    return Ok(Step::Wake);
                }
            }
            TakeAt::Want => {
                let out = self.out.expect("a take decides before it sets its want");
                m.store(at(slot, CORES_WANT), self.want - out.granted);
                return Ok(Step::Done(out));
            }
        }
        Ok(Step::Next)
    }
}

/// Gives `cores` of `slot`'s back: a pool that shrank.
pub struct GiveCores {
    slot: Slot,
    cores: u64,
    at: Lowering,
    holds: u64,
}

impl GiveCores {
    pub fn new(slot: Slot, cores: u64) -> GiveCores {
        GiveCores { slot, cores, at: Lowering::Own, holds: 0 }
    }
}

impl Op for GiveCores {
    /// What the slot holds after.
    type Out = u64;
    const NAME: &'static str = "a give of the machine's cores";

    fn step<A: Alive>(&mut self, m: &Machine<'_>, _: &A) -> Result<Step<u64>, MachineError> {
        lower(m, self.slot, self.cores, Charge::Cores, &mut self.at, &mut self.holds)
    }
}

// --- Reading it ------------------------------------------------------------------------------

/// The live slots, summed (the dead freed on the way): for a status line, and for a process with
/// memory it could give back to see what the others want.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Counted {
    pub processes: u32,
    pub pruned: u32,
    pub reserved: u64,
    pub wanted: u64,
    pub cores_held: u64,
    pub cores_wanted: u64,
    pub budget: u64,
    pub cores: u64,
}

pub struct Census {
    scan: Scan,
    budget: Option<u64>,
}

impl Census {
    pub fn new() -> Census {
        Census { scan: Scan::new(&[RESERVED, WANT, CORES, CORES_WANT]), budget: None }
    }
}

impl Default for Census {
    fn default() -> Census {
        Census::new()
    }
}

impl Op for Census {
    type Out = Counted;
    const NAME: &'static str = "a census of the machine's ledger";

    fn step<A: Alive>(&mut self, m: &Machine<'_>, alive: &A) -> Result<Step<Counted>, MachineError> {
        if !matches!(self.scan.at, ScanAt::End) {
            return Ok(match self.scan.step(m, alive)? {
                Scanned::Wake => Step::Wake,
                Scanned::Next | Scanned::Done => Step::Next,
            });
        }
        let Some(budget) = self.budget else {
            self.budget = Some(m.load(AT_BUDGET));
            return Ok(Step::Next);
        };
        let [reserved, wanted, cores_held, cores_wanted] = self.scan.sums;
        Ok(Step::Done(Counted {
            processes: self.scan.live,
            pruned: self.scan.pruned,
            reserved,
            wanted,
            cores_held,
            cores_wanted,
            budget,
            cores: m.load(AT_CORES),
        }))
    }
}

// --- Errors ----------------------------------------------------------------------------------

/// Which count of a slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Charge {
    Bytes,
    Cores,
}

impl Charge {
    fn field(self) -> usize {
        match self {
            Charge::Bytes => RESERVED,
            Charge::Cores => CORES,
        }
    }
}

impl fmt::Display for Charge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Charge::Bytes => "bytes",
            Charge::Cores => "cores",
        })
    }
}

/// What went wrong, and in which ledger file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MachineError {
    pub path: PathBuf,
    pub why: Why,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The mapping is not [`WORDS`] words.
    Length { words: usize },
    Magic { found: u64 },
    Layout { found: u64 },
    /// A header word that is 0 where it may not be (budget, cores), or not 0 where it must be.
    Header { word: usize, found: u64 },
    /// An owner word in a state no layout has.
    Owner { slot: usize, word: u64 },
    /// A process of pid 0 joined.
    NoPid,
    /// Every one of the [`SLOTS`] slots holds a live process.
    Full,
    /// A process's slot is no longer its: a bug, as a live process is never freed.
    Lost { slot: usize, pid: u32, start: u64 },
    /// A process gave back more than its slot holds.
    Over { slot: usize, charge: Charge, holds: u64, gives: u64 },
    /// An acquire of more than the whole machine budget.
    TooBig { bytes: u64, budget: u64 },
    /// An operation that did not end in its bound of steps.
    Stuck { op: &'static str, steps: usize },
    /// An operation that sleeps run where none may.
    Blocks { op: &'static str },
    /// An acquire that tried again this many times in a row, the epoch moving each time.
    Churn { rounds: u32 },
}

impl fmt::Display for MachineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match self.why {
            Why::Length { words } => write!(
                f,
                "the machine's ledger {path} is {words} words, where layout {LAYOUT} is {WORDS}: \
                 another program's file, or a bug of the driver that mapped it"
            ),
            Why::Magic { found } => write!(
                f,
                "the machine's ledger {path} starts with {found:#018x}, not blaze's magic \
                 {MAGIC:#018x}: another program's file; once no blaze of this user runs, remove it"
            ),
            Why::Layout { found } => write!(
                f,
                "the machine's ledger {path} is of layout {found}, and this blaze reads layout \
                 {LAYOUT}: its name says the layout, so the file was written by another program or \
                 renamed; once no blaze of this user runs, remove it"
            ),
            Why::Header { word, found } => write!(
                f,
                "the machine's ledger {path} is corrupt: its header word {word} is {found}, which \
                 layout {LAYOUT} never writes there; once no blaze of this user runs, remove it"
            ),
            Why::Owner { slot, word } => write!(
                f,
                "the machine's ledger {path} is corrupt: slot {slot}'s owner word {word:#018x} is in \
                 no state layout {LAYOUT} has; once no blaze of this user runs, remove it"
            ),
            Why::NoPid => {
                write!(f, "a process of pid 0 was to join the machine's ledger {path}, where no process has pid 0")
            }
            Why::Full => write!(
                f,
                "every one of the {SLOTS} slots of the machine's ledger {path} holds a running blaze \
                 process of this user (the bound, `SLOTS`): this one is refused rather than run \
                 uncounted; let one of them end, then start this again"
            ),
            Why::Lost { slot, pid, start } => write!(
                f,
                "slot {slot} of the machine's ledger {path} is no longer this process's (pid {pid}, \
                 started at {start}): another process freed it as dead, which a running process \
                 never is"
            ),
            Why::Over { slot, charge, holds, gives } => write!(
                f,
                "slot {slot} of the machine's ledger {path} holds {holds} {charge}, and its process \
                 gave back {gives}"
            ),
            Why::TooBig { bytes, budget } => write!(
                f,
                "{bytes} bytes were asked of the machine's ledger {path}, past its whole budget of \
                 {budget}: no release makes them fit"
            ),
            Why::Stuck { op, steps } => write!(
                f,
                "{op} on the machine's ledger {path} did not end in {steps} steps, its bound"
            ),
            Why::Blocks { op } => {
                write!(f, "{op} on the machine's ledger {path} asked to sleep, where none may")
            }
            Why::Churn { rounds } => write!(
                f,
                "an acquire of the machine's memory in {path} was refused {rounds} times in a row \
                 with the ledger changing each time, and never slept: its bound"
            ),
        }
    }
}

impl std::error::Error for MachineError {}
