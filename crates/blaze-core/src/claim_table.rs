//! Records in flight: which task is writing the record of a key, who waits for it, and whether a
//! wait would close a cycle (docs/v2/plan/design/parallel.md, "End state (architect,
//! 2026-11-16)", point 3). There is no dependency graph: each slot knows its owner, and each
//! parked task the one slot it waits on.
//!
//! - **A slot per key**, in a fixed open-addressed table made once ([`ClaimTable::new_in`]); it
//!   goes empty -> reserving (its key being written) -> claimed by a task -> done with a value
//!   (a record's ordinal, or a failure's: the table does not read it), and never back. A full
//!   table is [`ClaimError::Full`], never a grow.
//! - **[`ClaimTable::claim`]**: the first asker of a key owns it ([`Claimed::Mine`]) and runs it
//!   (inline, help first); a later one finds it [`Claimed::Running`] or [`Claimed::Done`].
//! - **[`ClaimTable::wait`]**: a task that found a key running publishes its edge (the slot it
//!   waits on), then walks owner -> that owner's edge -> ... at most `tasks` steps. Reaching a
//!   slot it owns is [`Waited::Cycle`]. Else it pushes itself on the slot's waiter stack
//!   ([`Waited::Parked`]), or finds the stack closed and reads the value ([`Waited::Ready`]).
//!   A parked task holds no thread: the driver re-enters it when [`ClaimTable::publish`] hands
//!   it back.
//! - **[`ClaimTable::publish`]** stores the value, then swaps the waiter stack for CLOSED and
//!   returns every task that was on it ([`Woken`]), each with its edge cleared. A push before
//!   the swap is in what it returns; a push after it sees CLOSED and reads the value: no waiter
//!   is lost (the test `no_waiter_is_lost`, and its planted load-then-store close).
//! - **Two tasks closing one cycle at once**: each stores its edge before it reads the other's
//!   (all SeqCst), so at least one of them sees the cycle (the test `a_cycle_is_seen`, and its
//!   planted walk-before-edge).
//! - **Orderings**: every operation is SeqCst. A claim, a wait and a publish are a few atomic
//!   operations on 10 us to 10 ms tasks, and the cycle argument needs a single total order.
//! - **Bounded** (rule 18): a probe visits at most every slot; a wait on a key still being
//!   reserved spins at most [`SPIN_MOST`] rounds; a push retries at most [`SPIN_MOST`] times;
//!   the walk takes at most `tasks` steps. Each bound passed is a typed error naming the slot.
//!
//! Sans-IO: no clock, no thread, no allocation after `new_in`. NEVER COMPILED as written
//! (architect lane, 2026-11-16): the build integrator compiles it.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_own::Vec;

use crate::atomic::{AtomicU64, Ordering::SeqCst};

/// The most rounds a claim spins on a slot whose key is being written, and the most times a
/// push onto a waiter stack is retried after losing a race.
pub const SPIN_MOST: u32 = 1 << 10;

/// A record's key: the 32 bytes of its digest, as four words. The first word picks the probe's
/// start, so a key is expected to be a hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key(pub [u64; 4]);

/// A task's index in the table's task rows (below the `tasks` it was made with).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskId(pub u32);

/// A slot's index: what [`ClaimTable::wait`] and [`ClaimTable::publish`] take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct At(pub u32);

/// What a claim found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimed {
    /// The caller owns the key: it computes the record and publishes it.
    Mine(At),
    /// Another task owns it and has not published.
    Running { at: At, owner: TaskId },
    /// Published with this value.
    Done { at: At, value: u32 },
}

/// What a wait did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waited {
    /// On the slot's waiter stack: the publisher hands the task back.
    Parked,
    /// The slot was published first: its value.
    Ready(u32),
    /// Waiting would close a cycle through a slot the caller owns; nothing was pushed and the
    /// caller's edge is cleared. [`ClaimTable::chain`] names the cycle from `at`.
    Cycle { at: At },
}

/// A broken bug-seeding plan for the tests' controls: each weakens one guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimPlant {
    None,
    /// A claim reserves an empty slot by a load and a store, not a compare-exchange.
    ReserveByStore,
    /// A wait walks the owners before it publishes its own edge.
    WalkBeforeEdge,
    /// A publish reads the waiter stack and then stores CLOSED, not one swap.
    CloseByLoad,
}

/// Why the table refused (rule 19: each names the slot or the bound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimError {
    /// Every slot holds another key.
    Full { capacity: u32 },
    /// The slot's key was still being written after [`SPIN_MOST`] rounds.
    Reserving { at: At },
    /// A push onto the slot's waiter stack lost [`SPIN_MOST`] races.
    Contended { at: At },
    /// A publish or wait of a slot the task does not own or that is not claimed.
    NotOwner { at: At, task: TaskId },
    /// A task id at or past the table's task rows.
    NoSuchTask { task: TaskId, tasks: u32 },
    /// A slot index past the table.
    NoSuchSlot { at: At, capacity: u32 },
    /// The table's room could not be had.
    Memory { slots: u32, tasks: u32 },
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ClaimError::Full { capacity } => write!(f, "the claim table's {capacity} slots all hold other keys (rule 18: a table has a cap)"),
            ClaimError::Reserving { at } => write!(f, "claim slot {}: its key was still being written after {SPIN_MOST} rounds (rule 18)", at.0),
            ClaimError::Contended { at } => write!(f, "claim slot {}: a wait lost {SPIN_MOST} pushes onto its waiters (rule 18)", at.0),
            ClaimError::NotOwner { at, task } => write!(f, "claim slot {}: task {} does not own it, or it is not claimed (rule 3)", at.0, task.0),
            ClaimError::NoSuchTask { task, tasks } => write!(f, "task {} is past the claim table's {tasks} task rows", task.0),
            ClaimError::NoSuchSlot { at, capacity } => write!(f, "claim slot {} is past the table's {capacity} slots", at.0),
            ClaimError::Memory { slots, tasks } => write!(f, "the claim table's room ({slots} slots, {tasks} task rows) could not be had"),
        }
    }
}

impl core::error::Error for ClaimError {}

// A slot's state word: the tag in the top two bits, a task or a value in the low 32.
const TAG: u32 = 62;
const EMPTY: u64 = 0;
const RESERVING: u64 = 1;
const CLAIMED: u64 = 2;
const DONE: u64 = 3;

const fn state(tag: u64, low: u32) -> u64 {
    (tag << TAG) | low as u64
}

const fn tag_of(word: u64) -> u64 {
    word >> TAG
}

const fn low_of(word: u64) -> u32 {
    word as u32
}

// A waiter stack's head: empty, closed by the publish, or a task's index plus one.
const NO_WAITER: u64 = 0;
const CLOSED: u64 = u64::MAX;

struct Slot {
    key: [AtomicU64; 4],
    state: AtomicU64,
    waiters: AtomicU64,
}

/// A task's row, written by that task (its edge, its link) and cleared by the publisher that
/// wakes it; on its own line so tasks do not share one.
#[repr(align(128))]
struct TaskRow {
    /// The slot it waits on, plus one; 0 when it waits on none.
    waiting_on: AtomicU64,
    /// The next task down the waiter stack it is on, plus one; 0 at the bottom.
    next: AtomicU64,
}

/// The table. See the module's doc.
pub struct ClaimTable<A: Allocator> {
    slots: Vec<Slot, A>,
    tasks: Vec<TaskRow, A>,
    mask: u32,
    plant: ClaimPlant,
}

impl<A: Allocator> ClaimTable<A> {
    /// A table of at least `slots` slots (rounded up to a power of two) for task ids below
    /// `tasks`, its room taken from `alloc` now and never again.
    pub fn new_in(slots: u32, tasks: u32, alloc: A) -> Result<ClaimTable<A>, ClaimError>
    where
        A: Clone,
    {
        Self::planted_in(slots, tasks, ClaimPlant::None, alloc)
    }

    /// [`ClaimTable::new_in`] with a plant (the tests' controls).
    pub fn planted_in(slots: u32, tasks: u32, plant: ClaimPlant, alloc: A) -> Result<ClaimTable<A>, ClaimError>
    where
        A: Clone,
    {
        let refused = ClaimError::Memory { slots, tasks };
        let capacity = slots.max(1).checked_next_power_of_two().ok_or(refused)?;
        let mut slot_rows = Vec::try_with_capacity_in(capacity as usize, alloc.clone()).map_err(|_| refused)?;
        for _ in 0..capacity {
            let slot = Slot {
                key: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
                state: AtomicU64::new(state(EMPTY, 0)),
                waiters: AtomicU64::new(NO_WAITER),
            };
            slot_rows.try_push(slot).map_err(|_| refused)?;
        }
        let mut task_rows = Vec::try_with_capacity_in(tasks as usize, alloc).map_err(|_| refused)?;
        for _ in 0..tasks {
            task_rows.try_push(TaskRow { waiting_on: AtomicU64::new(0), next: AtomicU64::new(0) }).map_err(|_| refused)?;
        }
        Ok(ClaimTable { slots: slot_rows, tasks: task_rows, mask: capacity - 1, plant })
    }

    /// The number of slots.
    pub fn capacity(&self) -> u32 {
        self.mask + 1
    }

    fn task(&self, task: TaskId) -> Result<&TaskRow, ClaimError> {
        self.tasks.get(task.0 as usize).ok_or(ClaimError::NoSuchTask { task, tasks: self.tasks.len() as u32 })
    }

    fn slot(&self, at: At) -> Result<&Slot, ClaimError> {
        self.slots.get(at.0 as usize).ok_or(ClaimError::NoSuchSlot { at, capacity: self.capacity() })
    }

    fn holds(slot: &Slot, key: Key) -> bool {
        slot.key.iter().zip(key.0).all(|(word, want)| word.load(SeqCst) == want)
    }

    /// The key's slot for `me`: taken if the key is new (`Mine`), else how it stands.
    pub fn claim(&self, key: Key, me: TaskId) -> Result<Claimed, ClaimError> {
        self.task(me)?;
        let start = key.0[0] as u32 & self.mask;
        for probe in 0..self.capacity() {
            let at = At(start.wrapping_add(probe) & self.mask);
            let slot = &self.slots[at.0 as usize];
            let mut spins = 0;
            loop {
                let word = slot.state.load(SeqCst);
                match tag_of(word) {
                    EMPTY => {
                        let reserved = match self.plant {
                            ClaimPlant::ReserveByStore => {
                                slot.state.store(state(RESERVING, me.0), SeqCst);
                                true
                            }
                            _ => slot.state.compare_exchange(word, state(RESERVING, me.0), SeqCst, SeqCst).is_ok(),
                        };
                        if !reserved {
                            // Another task reserved it first: look at it again.
                            continue;
                        }
                        for (word, value) in slot.key.iter().zip(key.0) {
                            word.store(value, SeqCst);
                        }
                        slot.state.store(state(CLAIMED, me.0), SeqCst);
                        return Ok(Claimed::Mine(at));
                    }
                    RESERVING => {
                        spins += 1;
                        if spins > SPIN_MOST {
                            return Err(ClaimError::Reserving { at });
                        }
                        core::hint::spin_loop();
                    }
                    tag => {
                        if !Self::holds(slot, key) {
                            break;
                        }
                        return Ok(match tag {
                            CLAIMED => Claimed::Running { at, owner: TaskId(low_of(word)) },
                            _ => Claimed::Done { at, value: low_of(word) },
                        });
                    }
                }
            }
        }
        Err(ClaimError::Full { capacity: self.capacity() })
    }

    /// Whether the walk from slot `at` reaches a slot `me` owns: owner -> the owner's edge ->
    /// ..., at most one step per task row.
    fn closes(&self, at: At, me: TaskId) -> bool {
        let mut cur = at;
        for _ in 0..self.tasks.len() {
            let Some(slot) = self.slots.get(cur.0 as usize) else { return false };
            let word = slot.state.load(SeqCst);
            if tag_of(word) != CLAIMED {
                return false;
            }
            let owner = TaskId(low_of(word));
            if owner == me {
                return true;
            }
            let Some(row) = self.tasks.get(owner.0 as usize) else { return false };
            let edge = row.waiting_on.load(SeqCst);
            if edge == 0 {
                return false;
            }
            cur = At((edge - 1) as u32);
        }
        false
    }

    /// `me` waits for slot `at`, which a claim found running. See the module's doc.
    pub fn wait(&self, at: At, me: TaskId) -> Result<Waited, ClaimError> {
        let row = self.task(me)?;
        let slot = self.slot(at)?;
        let edge = u64::from(at.0) + 1;
        let cycle = match self.plant {
            ClaimPlant::WalkBeforeEdge => {
                let cycle = self.closes(at, me);
                row.waiting_on.store(edge, SeqCst);
                cycle
            }
            _ => {
                row.waiting_on.store(edge, SeqCst);
                self.closes(at, me)
            }
        };
        if cycle {
            row.waiting_on.store(0, SeqCst);
            return Ok(Waited::Cycle { at });
        }
        for _ in 0..SPIN_MOST {
            let head = slot.waiters.load(SeqCst);
            if head == CLOSED {
                row.waiting_on.store(0, SeqCst);
                let word = slot.state.load(SeqCst);
                return Ok(Waited::Ready(low_of(word)));
            }
            row.next.store(head, SeqCst);
            if slot.waiters.compare_exchange(head, u64::from(me.0) + 1, SeqCst, SeqCst).is_ok() {
                return Ok(Waited::Parked);
            }
        }
        row.waiting_on.store(0, SeqCst);
        Err(ClaimError::Contended { at })
    }

    /// `me`, the owner of slot `at`, publishes `value` (a record's ordinal, or a failure's):
    /// the tasks that waited are returned, to be re-entered.
    pub fn publish(&self, at: At, me: TaskId, value: u32) -> Result<Woken<'_, A>, ClaimError> {
        self.task(me)?;
        let slot = self.slot(at)?;
        let word = slot.state.load(SeqCst);
        if word != state(CLAIMED, me.0) {
            return Err(ClaimError::NotOwner { at, task: me });
        }
        slot.state.store(state(DONE, value), SeqCst);
        let head = match self.plant {
            ClaimPlant::CloseByLoad => {
                let head = slot.waiters.load(SeqCst);
                slot.waiters.store(CLOSED, SeqCst);
                head
            }
            _ => slot.waiters.swap(CLOSED, SeqCst),
        };
        Ok(Woken { table: self, next: head })
    }

    /// The owners met walking from slot `at` (owner, its edge, ...), at most one per task row:
    /// how a [`Waited::Cycle`] is named. Each item is a slot and its owner then.
    pub fn chain(&self, at: At) -> Chain<'_, A> {
        Chain { table: self, cur: Some(at), left: self.tasks.len() }
    }
}

/// The tasks a publish took off a slot's waiter stack, each with its edge cleared as it is
/// handed out (its link read first, so the task may wait again at once).
pub struct Woken<'t, A: Allocator> {
    table: &'t ClaimTable<A>,
    next: u64,
}

impl<A: Allocator> Iterator for Woken<'_, A> {
    type Item = TaskId;

    fn next(&mut self) -> Option<TaskId> {
        if self.next == NO_WAITER || self.next == CLOSED {
            return None;
        }
        let task = TaskId((self.next - 1) as u32);
        let row = self.table.tasks.get(task.0 as usize)?;
        self.next = row.next.load(SeqCst);
        row.waiting_on.store(0, SeqCst);
        Some(task)
    }
}

/// See [`ClaimTable::chain`].
pub struct Chain<'t, A: Allocator> {
    table: &'t ClaimTable<A>,
    cur: Option<At>,
    left: usize,
}

impl<A: Allocator> Iterator for Chain<'_, A> {
    type Item = (At, TaskId);

    fn next(&mut self) -> Option<(At, TaskId)> {
        let at = self.cur.take()?;
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let word = self.table.slots.get(at.0 as usize)?.state.load(SeqCst);
        if tag_of(word) != CLAIMED {
            return None;
        }
        let owner = TaskId(low_of(word));
        let edge = self.table.tasks.get(owner.0 as usize)?.waiting_on.load(SeqCst);
        if edge != 0 {
            self.cur = Some(At((edge - 1) as u32));
        }
        Some((at, owner))
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    //! Run: `blaze test -p blaze-core claim_table` (or cargo's). Every test prints nothing on a
    //! pass; `cargo test` reports `test claim_table::tests::... ok` for each of the seven.

    use std::alloc::Global;

    use blaze_sim::{Broke, Config, Tape, check};

    use super::{At, ClaimPlant, ClaimTable, Claimed, Key, TaskId, Waited};
    use crate::world::threads::{self, Ended, Step};

    const K: Key = Key([5, 6, 7, 8]);
    const L: Key = Key([5, 9, 9, 9]);

    fn table(plant: ClaimPlant) -> ClaimTable<Global> {
        ClaimTable::planted_in(8, 4, plant, Global).expect("a small table")
    }

    fn s(n: usize) -> impl Iterator<Item = Step> {
        (0..n).map(|_| Step { second: false, pick: 0 })
    }

    fn d(n: usize) -> impl Iterator<Item = Step> {
        (0..n).map(|_| Step { second: true, pick: 0 })
    }

    fn drawn(tape: &Tape) -> std::vec::Vec<Step> {
        (0..tape.int_in(0..=48u64, "steps")).map(|_| Step { second: tape.coin("the second goes"), pick: 0 }).collect()
    }

    /// One thread: a claim, a second asker finds it running and parks, the publish hands it
    /// back, and a third asker finds it done; a colliding key takes the next slot.
    #[test]
    fn one_thread_claim_wait_publish() {
        let t = table(ClaimPlant::None);
        let Ok(Claimed::Mine(at)) = t.claim(K, TaskId(0)) else { panic!("the first claim owns the key") };
        assert_eq!(t.claim(K, TaskId(1)), Ok(Claimed::Running { at, owner: TaskId(0) }));
        assert_eq!(t.wait(at, TaskId(1)), Ok(Waited::Parked));
        let Ok(Claimed::Mine(other)) = t.claim(L, TaskId(2)) else { panic!("a colliding key owns its own slot") };
        assert_ne!(other, at);
        let woken: std::vec::Vec<TaskId> = t.publish(at, TaskId(0), 7).expect("the owner publishes").collect();
        assert_eq!(woken, [TaskId(1)]);
        assert_eq!(t.claim(K, TaskId(3)), Ok(Claimed::Done { at, value: 7 }));
        assert!(t.publish(at, TaskId(0), 8).is_err(), "a second publish is refused");
    }

    /// Two claims of one new key: when both end, exactly one owns it.
    fn race(plant: ClaimPlant, schedule: &[Step]) -> Option<(Claimed, Claimed)> {
        let t = table(plant);
        match threads::two(schedule, || t.claim(K, TaskId(0)), || t.claim(K, TaskId(1))) {
            (Ended::Done(Ok(a)), Ended::Done(Ok(b))) => Some((a, b)),
            _ => None,
        }
    }

    fn one_owner(got: Option<(Claimed, Claimed)>) -> bool {
        !matches!(got, Some((Claimed::Mine(_), Claimed::Mine(_))))
    }

    /// The publisher and a waiter: a waiter that parked is among those the publish returned.
    fn publish_and_wait(plant: ClaimPlant, schedule: &[Step]) -> (Option<Waited>, Option<std::vec::Vec<TaskId>>) {
        let t = table(plant);
        let Ok(Claimed::Mine(at)) = t.claim(K, TaskId(0)) else { panic!("set-up claim") };
        let (published, waited) = threads::two(
            schedule,
            || t.publish(at, TaskId(0), 7).map(|woken| woken.collect::<std::vec::Vec<TaskId>>()),
            || t.wait(at, TaskId(1)),
        );
        let woken = match published {
            Ended::Done(Ok(woken)) => Some(woken),
            _ => None,
        };
        let waited = match waited {
            Ended::Done(Ok(waited)) => Some(waited),
            _ => None,
        };
        (waited, woken)
    }

    fn none_lost(got: &(Option<Waited>, Option<std::vec::Vec<TaskId>>)) -> bool {
        match got {
            (Some(Waited::Parked), Some(woken)) => woken.contains(&TaskId(1)),
            (Some(Waited::Ready(value)), _) => *value == 7,
            (Some(Waited::Cycle { .. }), _) => false,
            _ => true,
        }
    }

    /// Tasks 0 and 1 own one slot each and wait for each other's at once.
    fn cross_wait(plant: ClaimPlant, schedule: &[Step]) -> Option<(Waited, Waited)> {
        let t = table(plant);
        let Ok(Claimed::Mine(a)) = t.claim(K, TaskId(0)) else { panic!("set-up claim of a") };
        let Ok(Claimed::Mine(b)) = t.claim(L, TaskId(1)) else { panic!("set-up claim of b") };
        match threads::two(schedule, || t.wait(b, TaskId(0)), || t.wait(a, TaskId(1))) {
            (Ended::Done(Ok(x)), Ended::Done(Ok(y))) => Some((x, y)),
            _ => None,
        }
    }

    fn seen(got: Option<(Waited, Waited)>) -> bool {
        !matches!(got, Some((Waited::Parked, Waited::Parked)))
    }

    /// Drawn schedules of all three pairs hold their oracles.
    #[test]
    fn claims_waits_and_cycles_hold_on_drawn_schedules() {
        let config = Config { must_reach: &["a waiter parked and was woken", "a waiter found the slot published", "a cycle was seen"], ..Config::cases(3_000) };
        check(config, |tape| {
            let schedule = drawn(tape);
            match tape.int_in(0..=2u8, "which pair") {
                0 => match one_owner(race(ClaimPlant::None, &schedule)) {
                    true => Ok(()),
                    false => Err(Broke::Oracle(format!("two owners of one key: {schedule:?}"))),
                },
                1 => {
                    let got = publish_and_wait(ClaimPlant::None, &schedule);
                    match got.0 {
                        Some(Waited::Parked) if got.1.is_some() => tape.reached("a waiter parked and was woken"),
                        Some(Waited::Ready(_)) => tape.reached("a waiter found the slot published"),
                        _ => {}
                    }
                    match none_lost(&got) {
                        true => Ok(()),
                        false => Err(Broke::Oracle(format!("a waiter was lost or misread: {got:?}, {schedule:?}"))),
                    }
                }
                _ => {
                    let got = cross_wait(ClaimPlant::None, &schedule);
                    if matches!(got, Some((Waited::Cycle { .. }, _)) | Some((_, Waited::Cycle { .. }))) {
                        tape.reached("a cycle was seen");
                    }
                    match seen(got) {
                        true => Ok(()),
                        false => Err(Broke::Oracle(format!("both parked on a cycle: {schedule:?}"))),
                    }
                }
            }
        });
    }

    /// Control: a reservation by load then store gives two owners on the schedule load, load,
    /// then each runs to its end; the compare-exchange does not.
    #[test]
    fn the_oracle_catches_a_reservation_by_store() {
        let schedule: std::vec::Vec<Step> = s(1).chain(d(1)).chain(s(8)).chain(d(8)).collect();
        assert!(!one_owner(race(ClaimPlant::ReserveByStore, &schedule)));
        assert!(one_owner(race(ClaimPlant::None, &schedule)));
    }

    /// Control: a close by load then store loses a waiter that pushes between them: the
    /// publisher stores DONE and reads the empty stack (3 steps), the waiter publishes its edge,
    /// sees DONE, reads the stack still open and pushes (5 steps), then the publisher closes.
    #[test]
    fn the_oracle_catches_a_close_by_load() {
        let schedule: std::vec::Vec<Step> = s(3).chain(d(5)).chain(s(6)).chain(d(4)).collect();
        assert!(!none_lost(&publish_and_wait(ClaimPlant::CloseByLoad, &schedule)));
        assert!(none_lost(&publish_and_wait(ClaimPlant::None, &schedule)));
    }

    /// Control: walking before the edge is stored lets both miss the cycle when their walks
    /// interleave ahead of both stores.
    #[test]
    fn the_oracle_catches_a_walk_before_the_edge() {
        let schedule: std::vec::Vec<Step> = s(1).chain(d(1)).chain(s(1)).chain(d(1)).chain(s(6)).chain(d(6)).collect();
        assert!(!seen(cross_wait(ClaimPlant::WalkBeforeEdge, &schedule)));
        assert!(seen(cross_wait(ClaimPlant::None, &schedule)));
    }

    /// A cycle is named: from the slot a waits on, the chain meets b's owner, then a's.
    #[test]
    fn a_cycle_is_named_by_its_chain() {
        let t = table(ClaimPlant::None);
        let Ok(Claimed::Mine(a)) = t.claim(K, TaskId(0)) else { panic!("claim a") };
        let Ok(Claimed::Mine(b)) = t.claim(L, TaskId(1)) else { panic!("claim b") };
        assert_eq!(t.wait(a, TaskId(1)), Ok(Waited::Parked));
        assert_eq!(t.wait(b, TaskId(0)), Ok(Waited::Cycle { at: b }));
        let chain: std::vec::Vec<(At, TaskId)> = t.chain(b).collect();
        assert_eq!(chain, [(b, TaskId(1)), (a, TaskId(0))]);
    }
}
