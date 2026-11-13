//! The Budget: the one gate every commit of task memory passes, so the ceiling holds by
//! construction (docs/v2/plan/design/end-state/memory.md 4.2 and 9, as decided 2026-11-13).
//!
//! A sans-IO core: events in, decisions out, no clock, no file, no thread. Its driver (the frame's
//! chunk and span commits, the task heap, the scheduler's safe points) asks before it commits and
//! does what the answer says.
//!
//! Two owners, nothing else. **Core** is fixed at start (code, stacks, tables, the standing
//! frames) and **Task** is what a running task commits: its frames, its heap and the bytes it
//! dirtied in place. There is no page pool: the kernel's page cache is the one cache. Beside
//! them the Budget sets aside two named bounds that are never asked for page by page: the
//! **ungated reserve** (thread stacks, the system malloc zones of C libraries, the wasm runtime's
//! mappings) and one **overdraft per worker**.
//!
//! The rules:
//!
//! - Reservations are in quanta of the frame's chunk ([`QUANTUM`]); one [`Budget::reserve`] asks
//!   at most one worker's overdraft, so a refusal is always covered by it.
//! - A refusal is never an allocation failure: the allocation goes on from the asker's overdraft
//!   and the asker, or the tasks it outranks, unwind at their next safe point (a frame chunk
//!   boundary outside any lock, a `gives_way` read). Between two safe points a task reserves at
//!   most its worker's overdraft; past it is a broken contract ([`Misuse::NoSafePoint`]).
//! - Rank is (band, age): a lower band outranks a higher one (band 0 is work a client waits on,
//!   so speculative work never wounds it); within a band the elder outranks. The asker that
//!   outranks enough holders **wounds** them, youngest first; one that does not **gives way**.
//!   An elder whose quanta came from its overdraft waits at its next safe point until they are
//!   paid ([`SafePoint::Wait`]).
//! - A wounded task keeps its age when posted again and is not started while the elder it gave
//!   way to has not ended ([`Hold::Elder`]). A task the room holds back is started once the room
//!   it needs is free, and tasks it outranks wait behind it ([`Hold::Behind`]).
//! - A task that alone does not fit under the ceiling gets [`TooBig`], naming it; nothing else
//!   fails for memory.
//! - The machine's word on what this process may hold arrives as [`Budget::room`]; lowering it
//!   wounds the lowest-ranked holders until what they hold fits.
//! - No forecast enters a decision. [`Ended::peak`] is recorded for admission's throughput only.
//!
//! The invariants, each held by the simulation in `budget/tests.rs`: committed (core, the ungated
//! reserve, every task's reserved quanta and every overdraft in use) never passes the ceiling; and
//! every task that fits alone ends, wounded at most once per elder of its band.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::fmt;

use crate::chunks::CHUNK;

/// The unit of a reservation: one frame chunk, so a frame asks once per chunk it takes, never
/// once per allocation.
pub const QUANTUM: u64 = CHUNK as u64;

/// A task, by the caller's name for it; the same across the times it is posted again.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TaskId(pub u64);

/// A fair-share band: a lower band outranks a higher one whatever their ages. Band 0 is work a
/// client waits on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Band(pub u8);

/// A task's place in admission order, given by the caller and kept when the task is posted
/// again: a smaller age is an elder.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Age(pub u64);

/// What the Budget is fixed by at start. Bytes throughout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The most this process ever commits.
    pub ceiling: u64,
    /// The core owner: fixed at start, never asked for.
    pub core: u64,
    /// What one worker's task may commit past a refusal before its next safe point: a whole
    /// number of quanta.
    pub overdraft_per_worker: u64,
    pub workers: u32,
    /// The named reserve for what never passes the gate (thread stacks, C libraries' malloc
    /// zones, the wasm runtime's mappings).
    pub ungated: u64,
}

/// A [`Config`] the Budget refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigError {
    /// Core, the overdrafts and the ungated reserve leave no quantum for a task under the
    /// ceiling.
    DoesNotFit { ceiling: u64, core: u64, overdrafts: u64, ungated: u64 },
    /// The overdraft is not a positive whole number of quanta.
    OverdraftNotWhole { overdraft_per_worker: u64 },
    NoWorkers,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ConfigError::DoesNotFit { ceiling, core, overdrafts, ungated } => write!(
                f,
                "memory budget: core {core} B + overdrafts {overdrafts} B + ungated reserve {ungated} B \
                 leave no {QUANTUM} B quantum for a task under the ceiling of {ceiling} B"
            ),
            ConfigError::OverdraftNotWhole { overdraft_per_worker } => write!(
                f,
                "memory budget: an overdraft of {overdraft_per_worker} B per worker is not a positive \
                 multiple of the {QUANTUM} B quantum"
            ),
            ConfigError::NoWorkers => write!(f, "memory budget: no workers"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Why a task is not started yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hold {
    /// It gave way to this elder, which has not ended.
    Elder(TaskId),
    /// The room is short of the quanta it held when it gave way.
    Room { need: u64 },
    /// An elder the room holds back starts first.
    Behind(TaskId),
}

/// The answer to [`Budget::start`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Started {
    Running,
    /// Post it again when an event's `unheld` names it.
    Held { by: Hold },
}

/// A task that alone does not fit under the ceiling: a typed failure of that task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TooBig {
    pub task: TaskId,
    /// Bytes it asked to hold, this reservation included.
    pub wanted: u64,
    pub ceiling: u64,
}

impl fmt::Display for TooBig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "task {}: wants {} B, more than a task may hold alone under the memory ceiling of {} B",
            self.task.0, self.wanted, self.ceiling
        )
    }
}

/// The answer to [`Budget::reserve`]. Every answer but `Granted` hands the quanta from the
/// asker's overdraft: the allocation goes on, and what follows happens at its next safe point.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reserved {
    Granted,
    /// The asker is marked and gives way at its next safe point; `left` quanta of its overdraft
    /// remain until then.
    Overdrawn { left: u64 },
    /// The asker outranks enough holders: `victims` (youngest first, possibly none when tasks
    /// already wounded cover it) are marked. The asker is paid as they unwind and waits at its
    /// next safe point until it is.
    Wound { victims: Vec<TaskId>, left: u64 },
    /// The asker alone does not fit: it fails at its next safe point.
    TooBig(TooBig),
}

/// The answer to [`Budget::safe_point`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SafePoint {
    Go,
    /// Its overdraft is not paid yet: ask again after the next event that frees room; `victims`
    /// were marked to pay it.
    Wait { victims: Vec<TaskId> },
    /// Unwind, then [`Budget::wounded_at_safe_point`].
    GiveWay,
    /// Fail the task with this, then [`Budget::end`].
    Fail(TooBig),
}

/// What freeing room let start again: post each again.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Freed {
    pub unheld: Vec<TaskId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ended {
    /// The most quanta the task held at once in its last run: for admission's throughput only.
    pub peak: u64,
    pub unheld: Vec<TaskId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unwound {
    pub unheld: Vec<TaskId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Roomed {
    /// Marked to bring what tasks hold under the new room, lowest rank first.
    pub victims: Vec<TaskId>,
    pub unheld: Vec<TaskId>,
}

/// A call its driver's contract does not allow: a bug in the driver, named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Misuse {
    Unknown { task: TaskId, call: &'static str },
    AlreadyRunning { task: TaskId },
    /// One reservation of more than a worker's overdraft: split the commit.
    AskOverOverdraft { task: TaskId, quanta: u64, overdraft: u64 },
    /// The task reserved past its overdraft before a safe point.
    NoSafePoint { task: TaskId, in_use: u64, asked: u64, overdraft: u64 },
    /// More overdraft in use than the workers' reserve: more tasks overdrawn than workers.
    OverdraftsSpent { task: TaskId, in_use: u64, reserve: u64 },
    ReleaseOverHeld { task: TaskId, quanta: u64, held: u64 },
    /// `wounded_at_safe_point` for a task not told to give way.
    NotGivingWay { task: TaskId },
    /// A call for a parked task (`call`) named one still running.
    Running { task: TaskId, call: &'static str },
}

impl fmt::Display for Misuse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Misuse::Unknown { task, call } => {
                write!(f, "memory budget: {call} of task {}, which is not started", task.0)
            }
            Misuse::AlreadyRunning { task } => {
                write!(f, "memory budget: start of task {}, which is running", task.0)
            }
            Misuse::AskOverOverdraft { task, quanta, overdraft } => write!(
                f,
                "memory budget: task {} asked {quanta} quanta in one reservation, more than a worker's \
                 overdraft of {overdraft} (one reservation asks at most the overdraft)",
                task.0
            ),
            Misuse::NoSafePoint { task, in_use, asked, overdraft } => write!(
                f,
                "memory budget: task {} asked {asked} quanta with {in_use} of its overdraft of \
                 {overdraft} in use (a task reaches a safe point within its overdraft)",
                task.0
            ),
            Misuse::OverdraftsSpent { task, in_use, reserve } => write!(
                f,
                "memory budget: task {} overdrew with {in_use} of the workers' {reserve} overdraft \
                 quanta in use (no more tasks are overdrawn than there are workers)",
                task.0
            ),
            Misuse::ReleaseOverHeld { task, quanta, held } => write!(
                f,
                "memory budget: task {} released {quanta} quanta holding {held}",
                task.0
            ),
            Misuse::NotGivingWay { task } => write!(
                f,
                "memory budget: task {} unwound at a safe point without being told to give way",
                task.0
            ),
            Misuse::Running { task, call } => write!(
                f,
                "memory budget: {call} of task {}, which is running, not parked",
                task.0
            ),
        }
    }
}

impl std::error::Error for Misuse {}

/// The order tasks outrank each other in: lower first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Rank {
    band: Band,
    age: Age,
    task: TaskId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mark {
    No,
    GiveWay(Hold),
    Fail(TooBig),
}

/// A started task.
#[derive(Clone, Copy, Debug)]
struct Running {
    rank: Rank,
    /// Quanta granted from the room.
    reserved: u64,
    /// Quanta from its overdraft: a debt to pay from the room while it is unmarked, given back
    /// as it unwinds while it is marked.
    over: u64,
    peak: u64,
    /// The most it held before it last gave way, when it started again after giving way (0 on
    /// a first start): the room is fitted to keep that much for it ([`Budget::needs`]), so it
    /// is not short again at the same point while it grows back to where it was.
    reach: u64,
    mark: Mark,
}

impl Running {
    const fn held(&self) -> u64 {
        self.reserved + self.over
    }
}

/// A task that unwound and is not started again, or was held at its start.
#[derive(Clone, Copy, Debug)]
struct Parked {
    rank: Rank,
    peak: u64,
    hold: Option<Hold>,
}

enum Decision {
    /// Indices into `running`, lowest rank first.
    Wound(Vec<usize>),
    GiveWay(Hold),
}

/// The gate. One per process; its driver serializes the calls.
pub struct Budget {
    config: Config,
    /// Core and the ungated reserve, bytes.
    fixed: u64,
    /// What the machine lets this process commit, bytes.
    room: u64,
    /// Started tasks, by rank, eldest first.
    running: Vec<Running>,
    parked: Vec<Parked>,
    /// Sums over `running`.
    reserved: u64,
    over: u64,
    /// `over` of unmarked tasks: owed from the room before any other grant.
    debt: u64,
    /// Quanta of `reserved` lent to the gate's lock-free front ([`Budget::lend`]): counted as
    /// held, by no task yet, so a grant from the front can never pass what the room allows.
    lent: u64,
    #[cfg(test)]
    pub(crate) plant: Plant,
}

/// A planted bug, for the simulation's controls (`budget/tests.rs`): each breaks one rule of the
/// module's list, and the oracle must catch it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "constructed by the tests' controls"))]
pub(crate) enum Plant {
    None,
    /// Every reservation is granted.
    SkipGate,
    /// Victims are chosen eldest first, the asker's elders among them.
    WoundEldest,
    /// A task is started whatever holds it.
    ReadmitUnderElder,
    /// The overdrafts are neither set aside from the room nor counted in committed.
    OverdraftUncounted,
    /// Nobody wounds: every asker short of room gives way, held by the room alone.
    WoundStorm,
}

impl Budget {
    /// A Budget for `config`, refusing one whose fixed parts leave no quantum for a task.
    pub fn new(config: Config) -> Result<Budget, ConfigError> {
        if config.workers == 0 {
            return Err(ConfigError::NoWorkers);
        }
        if config.overdraft_per_worker == 0 || !config.overdraft_per_worker.is_multiple_of(QUANTUM)
        {
            return Err(ConfigError::OverdraftNotWhole {
                overdraft_per_worker: config.overdraft_per_worker,
            });
        }
        let refused = ConfigError::DoesNotFit {
            ceiling: config.ceiling,
            core: config.core,
            overdrafts: config.overdraft_per_worker.saturating_mul(u64::from(config.workers)),
            ungated: config.ungated,
        };
        let fixed = config.core.checked_add(config.ungated).ok_or(refused)?;
        let overdrafts =
            config.overdraft_per_worker.checked_mul(u64::from(config.workers)).ok_or(refused)?;
        let set_aside = fixed.checked_add(overdrafts).ok_or(refused)?;
        if set_aside.checked_add(QUANTUM).is_none_or(|least| least > config.ceiling) {
            return Err(refused);
        }
        Ok(Budget {
            config,
            fixed,
            room: config.ceiling,
            running: Vec::new(),
            parked: Vec::new(),
            reserved: 0,
            over: 0,
            debt: 0,
            lent: 0,
            #[cfg(test)]
            plant: Plant::None,
        })
    }

    /// The config it was made with.
    pub fn config(&self) -> Config {
        self.config
    }

    /// How many tasks are started and not ended.
    pub fn running(&self) -> usize {
        self.running.len()
    }

    /// Whether any task is parked (held at its start, or unwound and not started again): a
    /// start may then be held.
    pub fn any_parked(&self) -> bool {
        !self.parked.is_empty()
    }

    /// The quanta `task` holds as the Budget counts them (granted and overdrawn), if running.
    pub fn held_by(&self, task: TaskId) -> Option<u64> {
        self.at(task).map(|at| self.running[at].held())
    }

    /// Whether `task` is parked: held at its start, or unwound and not started again.
    pub fn is_parked(&self, task: TaskId) -> bool {
        self.parked.iter().any(|it| it.rank.task == task)
    }

    /// Quanta lent to the front and not taken back.
    pub fn lent(&self) -> u64 {
        self.lent
    }

    /// Lends up to `most` quanta of free room to a lock-free front, counted as reserved by no
    /// task: only while nothing is owed or waited for (no debt, no marked task, no task the
    /// room holds back), so the front never takes room an elder is waiting for. Says how many.
    pub fn lend(&mut self, most: u64) -> u64 {
        let waited_for = self.debt > 0
            || self.running.iter().any(|it| it.mark != Mark::No)
            || self.parked.iter().any(|it| matches!(it.hold, Some(Hold::Room { .. } | Hold::Behind(_))));
        if waited_for {
            return 0;
        }
        let free = self.limit_q().saturating_sub(self.reserved + self.debt);
        let lent = free.min(most);
        self.reserved += lent;
        self.lent += lent;
        lent
    }

    /// The front gives back `quanta` it was lent and handed to no task.
    pub fn unlend(&mut self, quanta: u64) -> Freed {
        debug_assert!(quanta <= self.lent, "unlent {quanta} quanta of {} lent", self.lent);
        let quanta = quanta.min(self.lent);
        self.lent -= quanta;
        self.reserved -= quanta;
        let mut freed = Freed::default();
        self.settle(&mut freed.unheld);
        freed
    }

    /// What `task` took from the front (`delta > 0`) or gave back to it (`delta < 0`) since
    /// it was last absorbed: moved between the lent count and the task's.
    pub fn absorb(&mut self, task: TaskId, delta: i64) -> Result<(), Misuse> {
        if delta == 0 {
            return Ok(());
        }
        let at = self.at(task).ok_or(Misuse::Unknown { task, call: "absorb" })?;
        let quanta = delta.unsigned_abs();
        if delta > 0 {
            if quanta > self.lent {
                return Err(Misuse::ReleaseOverHeld { task, quanta, held: self.lent });
            }
            self.lent -= quanta;
            let it = &mut self.running[at];
            it.reserved += quanta;
            it.peak = it.peak.max(it.held());
            return Ok(());
        }
        let it = &mut self.running[at];
        if quanta > it.held() {
            return Err(Misuse::ReleaseOverHeld { task, quanta, held: it.held() });
        }
        // Given back to the front: out of the task (its overdraft first), into what is lent.
        let from_over = quanta.min(it.over);
        it.over -= from_over;
        it.reserved -= quanta - from_over;
        if it.mark == Mark::No {
            self.debt -= from_over;
        }
        self.over -= from_over;
        self.reserved += from_over;
        self.lent += quanta;
        Ok(())
    }

    /// Bytes committed by what the Budget answers for: core, the ungated reserve, every task's
    /// reserved quanta and every overdraft in use. Never past the ceiling.
    pub fn committed(&self) -> u64 {
        let over = if self.planted(Plant::OverdraftUncounted) { 0 } else { self.over };
        self.fixed + (self.reserved + over) * QUANTUM
    }

    pub fn ceiling(&self) -> u64 {
        self.config.ceiling
    }

    /// What this process may commit now: the ceiling, or less while the machine says so.
    pub fn limit(&self) -> u64 {
        self.config.ceiling.min(self.room)
    }

    /// The most quanta one task may hold alone under the ceiling: past it, [`TooBig`].
    pub fn largest_task(&self) -> u64 {
        self.tasks_room(self.config.ceiling)
    }

    /// The least room, bytes, under which the Budget marks or holds anything now: core and the
    /// ungated reserve, every worker's overdraft (set aside from the room, [`Budget::limit`]),
    /// every quantum running tasks hold, owe or lent to the front, what a task started again
    /// after giving way still has to grow back to its reach, and the most a task the room holds
    /// back needs. A room at or above it marks no running task ([`Budget::room`]) and lets every
    /// task the room holds go as room is freed; a task started again finds room up to where it
    /// gave way, so it is not short at the same point twice. It is what the machine's ledger
    /// must hold for this process before anything is short: not [`Budget::committed`], which
    /// leaves the overdrafts out, so a room fitted to it is short by every worker's overdraft
    /// whatever the tasks hold, and wounds every holder each time it is given (2026-11-13: a
    /// hello-world's lowering wounded 1,280 times in 45 s).
    pub fn needs(&self) -> u64 {
        let held_back = self
            .parked
            .iter()
            .filter_map(|it| match it.hold {
                Some(Hold::Room { need }) => Some(need),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let regrow: u64 = self.running.iter().map(|it| it.reach.saturating_sub(it.held())).sum();
        let quanta = self.reserved + self.debt + held_back + regrow;
        self.set_aside().saturating_add(quanta.saturating_mul(QUANTUM))
    }

    /// A task enters, at its rank. A task wounded before keeps its age.
    pub fn start(&mut self, task: TaskId, band: Band, age: Age) -> Result<Started, Misuse> {
        if self.at(task).is_some() {
            return Err(Misuse::AlreadyRunning { task });
        }
        let rank = Rank { band, age, task };
        let heeds = !self.planted(Plant::ReadmitUnderElder);
        let mut reach = 0;
        if let Some(at) = self.parked.iter().position(|parked| parked.rank.task == task) {
            match self.parked[at].hold {
                Some(by) if heeds => return Ok(Started::Held { by }),
                _ => reach = self.parked.swap_remove(at).peak,
            }
        }
        // An elder the room holds back starts before any task it outranks, or younger tasks
        // could take each quantum freed before it fits.
        let first_held = self
            .parked
            .iter()
            .filter(|parked| matches!(parked.hold, Some(Hold::Room { .. })) && parked.rank < rank)
            .map(|parked| parked.rank.task)
            .min();
        if let Some(elder) = first_held
            && heeds
        {
            let by = Hold::Behind(elder);
            self.parked.push(Parked { rank, peak: reach, hold: Some(by) }); // alloc: a held task, once per hold
            return Ok(Started::Held { by });
        }
        let at = self.running.binary_search_by(|it| it.rank.cmp(&rank)).unwrap_or_else(|at| at);
        let row = Running { rank, reserved: 0, over: 0, peak: 0, reach, mark: Mark::No };
        self.running.insert(at, row); // alloc: a started task's row, grown to the most started at once
        Ok(Started::Running)
    }

    /// Asks `quanta` for `task` before they are committed (at most one worker's overdraft).
    pub fn reserve(&mut self, task: TaskId, quanta: u64) -> Result<Reserved, Misuse> {
        let at = self.at(task).ok_or(Misuse::Unknown { task, call: "reserve" })?;
        let overdraft = self.per_worker();
        if quanta > overdraft {
            return Err(Misuse::AskOverOverdraft { task, quanta, overdraft });
        }
        if self.planted(Plant::SkipGate) {
            self.grant(at, quanta);
            return Ok(Reserved::Granted);
        }
        self.pay_debts();
        if self.running[at].mark != Mark::No {
            let left = self.overdraw(at, quanta)?;
            return Ok(Reserved::Overdrawn { left });
        }
        if self.reserved + self.debt + quanta <= self.limit_q() {
            self.grant(at, quanta);
            return Ok(Reserved::Granted);
        }
        if let Some(misuse) = self.overdraw_refused(at, quanta) {
            return Err(misuse);
        }
        let wanted = self.running[at].held() + quanta;
        if wanted > self.largest_task() {
            let too_big =
                TooBig { task, wanted: wanted * QUANTUM, ceiling: self.config.ceiling };
            self.mark(at, Mark::Fail(too_big));
            self.overdraw(at, quanta)?;
            return Ok(Reserved::TooBig(too_big));
        }
        match self.decide(at, quanta) {
            Decision::Wound(victims) => {
                let victims = self.wound(&victims, Hold::Elder(task));
                let left = self.overdraw(at, quanta)?;
                Ok(Reserved::Wound { victims, left })
            }
            Decision::GiveWay(hold) => {
                self.mark(at, Mark::GiveWay(hold));
                let left = self.overdraw(at, quanta)?;
                Ok(Reserved::Overdrawn { left })
            }
        }
    }

    /// `task` gave back `quanta` it held (its overdraft first).
    pub fn release(&mut self, task: TaskId, quanta: u64) -> Result<Freed, Misuse> {
        let at = self.at(task).ok_or(Misuse::Unknown { task, call: "release" })?;
        let it = &mut self.running[at];
        if quanta > it.held() {
            return Err(Misuse::ReleaseOverHeld { task, quanta, held: it.held() });
        }
        let from_over = quanta.min(it.over);
        it.over -= from_over;
        it.reserved -= quanta - from_over;
        if it.mark == Mark::No {
            self.debt -= from_over;
        }
        self.over -= from_over;
        self.reserved -= quanta - from_over;
        let mut freed = Freed::default();
        self.settle(&mut freed.unheld);
        Ok(freed)
    }

    /// `task` is at a safe point: outside any lock, nothing half-built.
    pub fn safe_point(&mut self, task: TaskId) -> Result<SafePoint, Misuse> {
        let at = self.at(task).ok_or(Misuse::Unknown { task, call: "safe_point" })?;
        self.pay_debts();
        let it = self.running[at];
        Ok(match it.mark {
            Mark::GiveWay(_) => SafePoint::GiveWay,
            Mark::Fail(too_big) => SafePoint::Fail(too_big),
            Mark::No if it.over == 0 => SafePoint::Go,
            Mark::No => match self.decide(at, 0) {
                Decision::Wound(victims) => {
                    SafePoint::Wait { victims: self.wound(&victims, Hold::Elder(task)) }
                }
                Decision::GiveWay(hold) => {
                    self.mark(at, Mark::GiveWay(hold));
                    SafePoint::GiveWay
                }
            },
        })
    }

    /// A task told to give way has unwound: everything it held is back. It is parked until
    /// what holds it lets go.
    pub fn wounded_at_safe_point(&mut self, task: TaskId) -> Result<Unwound, Misuse> {
        let at = self.at(task).ok_or(Misuse::Unknown { task, call: "wounded_at_safe_point" })?;
        let Mark::GiveWay(hold) = self.running[at].mark else {
            return Err(Misuse::NotGivingWay { task });
        };
        let it = self.running.remove(at);
        self.reserved -= it.reserved;
        self.over -= it.over;
        let hold = match hold {
            Hold::Elder(elder) | Hold::Behind(elder) if !self.alive(elder) => None,
            hold => Some(hold),
        };
        self.parked.push(Parked { rank: it.rank, peak: it.peak, hold }); // alloc: a task that gave way, once per wound
        let mut unwound = Unwound { unheld: Vec::new() };
        self.settle(&mut unwound.unheld);
        Ok(unwound)
    }

    /// `task` is done, by success, failure or the driver giving it up while parked.
    pub fn end(&mut self, task: TaskId) -> Result<Ended, Misuse> {
        let peak = if let Some(at) = self.at(task) {
            let it = self.running.remove(at);
            self.reserved -= it.reserved;
            self.over -= it.over;
            if it.mark == Mark::No {
                self.debt -= it.over;
            }
            it.peak
        } else if let Some(at) = self.parked.iter().position(|it| it.rank.task == task) {
            self.parked.swap_remove(at).peak
        } else {
            return Err(Misuse::Unknown { task, call: "end" });
        };
        let mut ended = Ended { peak, unheld: Vec::new() };
        self.let_go_of(task, &mut ended.unheld);
        self.settle(&mut ended.unheld);
        Ok(ended)
    }

    /// The machine's word on what this process may commit, raised or lowered by another
    /// process. Lowered under what tasks hold, the lowest-ranked holders are marked.
    pub fn room(&mut self, bytes: u64) -> Roomed {
        self.room = bytes;
        let short = (self.reserved + self.debt).saturating_sub(self.limit_q());
        let mut roomed = Roomed { victims: Vec::new(), unheld: Vec::new() };
        if short > 0 {
            let mut cover = self.pending();
            let mut chosen = Vec::new();
            for (at, it) in self.running.iter().enumerate().rev() {
                if cover >= short {
                    break;
                }
                if it.mark == Mark::No && it.held() > 0 {
                    chosen.push(at); // alloc: a room's victims, once per lowering
                    cover += it.held();
                }
            }
            for at in chosen {
                let need = self.running[at].held();
                self.mark(at, Mark::GiveWay(Hold::Room { need }));
                roomed.victims.push(self.running[at].rank.task); // alloc: as above
            }
        }
        self.settle(&mut roomed.unheld);
        roomed
    }

    // --- The rules ------------------------------------------------------------------------

    /// Who covers what the asker at `at` is short by with `quanta` more: wounded tasks still
    /// unwinding, then unmarked holders it outranks, youngest first. If they cannot, it gives
    /// way to the youngest unmarked holder that outranks it, or to the room.
    fn decide(&self, at: usize, quanta: u64) -> Decision {
        let short = (self.reserved + self.debt + quanta).saturating_sub(self.limit_q());
        let asker = self.running[at].rank;
        let mut cover = self.pending();
        if !self.planted(Plant::WoundStorm) {
            let mut victims = Vec::new();
            let candidate = |it: &Running| it.mark == Mark::No && it.rank != asker && it.held() > 0;
            if self.planted(Plant::WoundEldest) {
                for (other, it) in self.running.iter().enumerate() {
                    if cover >= short {
                        break;
                    }
                    if candidate(it) {
                        victims.push(other); // alloc: a wound's victims, once per wound
                        cover += it.held();
                    }
                }
            } else {
                for (other, it) in self.running.iter().enumerate().rev() {
                    if cover >= short || it.rank <= asker {
                        break;
                    }
                    if candidate(it) {
                        victims.push(other); // alloc: a wound's victims, once per wound
                        cover += it.held();
                    }
                }
            }
            if cover >= short {
                return Decision::Wound(victims);
            }
        }
        let elder = match self.planted(Plant::WoundStorm) {
            true => None,
            false => self.running[..at]
                .iter()
                .rev()
                .find(|it| it.mark == Mark::No && it.held() > 0)
                .map(|it| it.rank.task),
        };
        Decision::GiveWay(match elder {
            Some(elder) => Hold::Elder(elder),
            None => Hold::Room { need: self.running[at].held() + quanta },
        })
    }

    fn wound(&mut self, victims: &[usize], hold: Hold) -> Vec<TaskId> {
        let mut named = Vec::with_capacity(victims.len()); // alloc: a wound's victims, once per wound
        for &at in victims {
            self.mark(at, Mark::GiveWay(hold));
            named.push(self.running[at].rank.task);
        }
        named
    }

    /// Pays unmarked tasks' overdrafts from free room, eldest first.
    fn pay_debts(&mut self) {
        if self.debt == 0 {
            return;
        }
        let limit = self.limit_q();
        for it in &mut self.running {
            if it.mark != Mark::No || it.over == 0 {
                continue;
            }
            let free = limit.saturating_sub(self.reserved);
            if free == 0 {
                break;
            }
            let paid = free.min(it.over);
            it.over -= paid;
            it.reserved += paid;
            self.over -= paid;
            self.debt -= paid;
            self.reserved += paid;
        }
    }

    /// After room was freed: debts first, then the tasks the room held whose need is free now,
    /// and those that waited behind them.
    fn settle(&mut self, unheld: &mut Vec<TaskId>) {
        self.pay_debts();
        let free = self.limit_q().saturating_sub(self.reserved + self.debt);
        let from = unheld.len();
        for it in &mut self.parked {
            if let Some(Hold::Room { need }) = it.hold
                && need <= free
            {
                it.hold = None;
                unheld.push(it.rank.task); // alloc: a task let start again, once per hold
            }
        }
        let room_let_go = unheld.len();
        for at in from..room_let_go {
            let elder = unheld[at];
            self.let_behind_go(elder, unheld);
        }
    }

    /// `task` ended: whoever gave way to it, or waited behind it, may start.
    fn let_go_of(&mut self, task: TaskId, unheld: &mut Vec<TaskId>) {
        for it in &mut self.parked {
            if it.hold == Some(Hold::Elder(task)) || it.hold == Some(Hold::Behind(task)) {
                it.hold = None;
                unheld.push(it.rank.task); // alloc: a task let start again, once per hold
            }
        }
    }

    fn let_behind_go(&mut self, elder: TaskId, unheld: &mut Vec<TaskId>) {
        for it in &mut self.parked {
            if it.hold == Some(Hold::Behind(elder)) {
                it.hold = None;
                unheld.push(it.rank.task); // alloc: a task let start again, once per hold
            }
        }
    }

    fn mark(&mut self, at: usize, mark: Mark) {
        let it = &mut self.running[at];
        if it.mark == Mark::No {
            self.debt -= it.over;
        }
        it.mark = mark;
    }

    fn grant(&mut self, at: usize, quanta: u64) {
        let it = &mut self.running[at];
        it.reserved += quanta;
        it.peak = it.peak.max(it.held());
        self.reserved += quanta;
    }

    fn overdraw_refused(&self, at: usize, quanta: u64) -> Option<Misuse> {
        let it = &self.running[at];
        let overdraft = self.per_worker();
        if it.over + quanta > overdraft {
            return Some(Misuse::NoSafePoint {
                task: it.rank.task,
                in_use: it.over,
                asked: quanta,
                overdraft,
            });
        }
        if self.over + quanta > self.overdrafts() {
            return Some(Misuse::OverdraftsSpent {
                task: it.rank.task,
                in_use: self.over,
                reserve: self.overdrafts(),
            });
        }
        None
    }

    /// Hands `quanta` from the overdraft; says how much of it is left.
    fn overdraw(&mut self, at: usize, quanta: u64) -> Result<u64, Misuse> {
        if let Some(misuse) = self.overdraw_refused(at, quanta) {
            return Err(misuse);
        }
        let overdraft = self.per_worker();
        let it = &mut self.running[at];
        it.over += quanta;
        it.peak = it.peak.max(it.held());
        if it.mark == Mark::No {
            self.debt += quanta;
        }
        self.over += quanta;
        Ok(overdraft - it.over)
    }

    /// What marked tasks hold of the room: it comes back as they unwind.
    fn pending(&self) -> u64 {
        self.running.iter().filter(|it| it.mark != Mark::No).map(|it| it.reserved).sum()
    }

    fn at(&self, task: TaskId) -> Option<usize> {
        self.running.iter().position(|it| it.rank.task == task)
    }

    /// Started and not ended: running, or parked to run again.
    fn alive(&self, task: TaskId) -> bool {
        self.at(task).is_some() || self.parked.iter().any(|it| it.rank.task == task)
    }

    fn per_worker(&self) -> u64 {
        self.config.overdraft_per_worker / QUANTUM
    }

    fn overdrafts(&self) -> u64 {
        self.per_worker() * u64::from(self.config.workers)
    }

    /// Quanta tasks may reserve under `limit` bytes: what core, the ungated reserve and the
    /// overdrafts leave.
    fn tasks_room(&self, limit: u64) -> u64 {
        limit.saturating_sub(self.set_aside()) / QUANTUM
    }

    /// Bytes of the room no task reserves: core, the ungated reserve and the overdrafts.
    fn set_aside(&self) -> u64 {
        match self.planted(Plant::OverdraftUncounted) {
            true => self.fixed,
            false => self.fixed + self.overdrafts() * QUANTUM,
        }
    }

    fn limit_q(&self) -> u64 {
        self.tasks_room(self.limit())
    }

    fn planted(&self, plant: Plant) -> bool {
        #[cfg(test)]
        let planted = self.plant == plant;
        #[cfg(not(test))]
        let planted = {
            let _ = plant;
            false
        };
        planted
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;
