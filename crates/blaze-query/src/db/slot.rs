//! Write-once slots (docs/v2/plan/design/scheduler.md §3.2, step 3): a value a
//! maintainer publishes for one subject (a crate) as soon as it has it, taking `&` of the
//! database, not `&mut`, so publishing opens no view and cancels no reader.
//!
//! A slot is an input field's value: a query reads the field (an edge to its node, as for any
//! input) and then the slot. Before the slot is filled no reader can see it empty: a read
//! waits on the slot's flight in the database's [`Flights`](blaze_graph::pump::Flights), held
//! by the task that fills it ([`Slot::claim`]), so the wait suspends the reader's fiber and its
//! span names the slot and that task as owner. Every reader so saw only the one value, which is
//! why filling it needs no view: nothing that read it can be out of date.
//!
//! - The first fill stores the value ([`Filled::First`]); a fill of an equal value again is
//!   [`Filled::Again`]; a fill of a different value is [`SlotRefused`], naming the slot (rule
//!   18): a slot changes only by an ordinary input write of a new slot ([`Slot::full`]) through
//!   the field's setter, which opens a view as every write does.
//! - A read of a slot no task has claimed is [`SlotError::Unclaimed`], told from the claim's
//!   state before the read touches the flight (so a read can never hold the flight a claimer is
//!   about to take, making it [`SlotError::Busy`]); one whose filler ended without filling it is
//!   [`SlotError::Abandoned`]; one by the task that holds its claim is
//!   [`SlotError::ReadByItsFiller`]. Each names the slot: never a wait with no one to end it
//!   (rule 18), never a default.
//! - A claim given back for its filler to run again ([`SlotClaim::give_back`]: the filler's
//!   wait gave way and its loop will run it again, after the loop or, sooner, in another task
//!   that claims it anew) makes its readers give way too, as a wait that would close that
//!   cycle: each runs again once the filler has. Only a filler that will really run again
//!   gives its claim back; any other unwind drops it ([`SlotError::Abandoned`]).
//! - A maintainer whose pass failed marks every slot it left unfilled abandoned
//!   ([`Slot::abandon`]), so a reader reads [`SlotError::Abandoned`] at once, never gives way
//!   for a rerun that will not come (rule 18).

use std::{
    cell::UnsafeCell,
    fmt,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

use blaze_graph::{
    Key,
    pump::{Held, NotHeld},
};

use super::{Cancelled, Database, local};

/// What a slot is: the maintainer that fills it, what it holds and what of, as a person reads
/// them (`impls`, `the impl directory`, `crate core`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotName {
    pub maintainer: &'static str,
    pub what: &'static str,
    pub of: Box<str>,
}

impl fmt::Display for SlotName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "slot `{}` of {}, which maintainer `{}` fills",
            self.what, self.of, self.maintainer
        )
    }
}

/// How a fill went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filled {
    /// The slot was empty and holds the value now.
    First,
    /// The slot held an equal value already.
    Again,
}

/// A second fill of a slot with a value other than the one it holds (rule 21): two different
/// values under one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotRefused {
    pub slot: SlotName,
}

impl fmt::Display for SlotRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} was filled a second time with a different value: a write-once slot is filled once \
             per view, and changes only by a write of the input that holds it",
            self.slot
        )
    }
}

impl std::error::Error for SlotRefused {}

/// Why a read or a claim of a slot has no value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotError {
    /// Read before any task claimed it: its maintainer had not reached its subject, so the
    /// reader runs ahead of what it depends on.
    Unclaimed { slot: SlotName },
    /// Its filler ended (panicked, or gave up) without filling it.
    Abandoned { slot: SlotName },
    /// Claimed while another task holds it.
    Busy { slot: SlotName },
    /// Claimed again by the task that holds it: the claim it has is the one that fills it.
    ClaimedAgain { slot: SlotName },
    /// Claimed once it was filled.
    Full { slot: SlotName },
    /// Read by the task that holds its claim, before it filled it: its value would depend on
    /// itself.
    ReadByItsFiller { slot: SlotName },
}

impl fmt::Display for SlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlotError::Unclaimed { slot } => {
                write!(
                    f,
                    "{slot} was read before any task claimed it: its reader runs ahead of its maintainer"
                )
            }
            SlotError::Abandoned { slot } => {
                write!(f, "{slot} was given up by the task that claimed it, unfilled")
            }
            SlotError::Busy { slot } => write!(f, "{slot} was claimed while another task holds it"),
            SlotError::ClaimedAgain { slot } => {
                write!(
                    f,
                    "{slot} was claimed again by the task that holds it: it fills it through its first claim"
                )
            }
            SlotError::Full { slot } => write!(f, "{slot} was claimed once filled"),
            SlotError::ReadByItsFiller { slot } => {
                write!(
                    f,
                    "{slot} was read by the task that claimed it, before it filled it: its value would depend on itself"
                )
            }
        }
    }
}

impl std::error::Error for SlotError {}

/// A write-once slot of `T`: see the module.
pub struct Slot<T> {
    name: SlotName,
    /// The slot's flight: a serial no other slot of the process has, in a key no memo's flight
    /// has ([`key`]).
    serial: u64,
    /// Held only to write `value` once and to compare a second fill with it.
    #[expect(clippy::disallowed_types, reason = "held only to write a slot's value once and to compare a second fill")]
    writing: blaze_core::lock::Lock<()>,
    /// Stored with release once `value` is written; read with acquire before `value` is.
    full: AtomicBool,
    /// Where its claim is: [`UNCLAIMED`], [`CLAIMED`], [`GIVEN_BACK`] or [`ABANDONED`], stored
    /// with release once the flight is held (or before it is let go of), so a reader that loads
    /// it with acquire and finds [`UNCLAIMED`] never touches the flight.
    claim: AtomicU8,
    /// The holder's task ([`task`]) while [`CLAIMED`], 0 otherwise: a read by it is
    /// [`SlotError::ReadByItsFiller`], never a wait on itself.
    claimer: AtomicU64,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// No task claimed it yet.
const UNCLAIMED: u8 = 0;
/// A task holds its flight, and fills it.
const CLAIMED: u8 = 1;
/// Its claim was given back for its filler to run again: a reader gives way.
const GIVEN_BACK: u8 = 2;
/// Its claim was dropped unfilled: its filler panicked or gave up.
const ABANDONED: u8 = 3;

/// The task running here, as a slot records its claimer: its job's id, from 1, so 0 is none.
fn task() -> u64 {
    u64::from(blaze_graph::sched::job_here().0) + 1
}

// SAFETY: `value` is written once, under `writing`, before `full` is stored with release, and
// read only after `full` was loaded with acquire, by `&`; it is dropped by `Drop` (`&mut`).
// So `&Slot<T>` hands out `&T` across threads (`T: Sync`) and the slot moves its `T` (`T:
// Send`).
unsafe impl<T: Send + Sync> Sync for Slot<T> {}
// SAFETY: as above: the slot owns its `T`.
unsafe impl<T: Send> Send for Slot<T> {}

/// The next slot's serial, from 1.
static SERIAL: AtomicU64 = AtomicU64::new(1);

/// The flight key of slot `serial`: its first eight bytes a mix of the serial (the flight table
/// indexes by them), its last eight a tag no memo's flight key has (theirs are zero there).
fn key(serial: u64) -> Key {
    let mixed = serial.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut key = [0; 32];
    key[..8].copy_from_slice(&mixed.to_le_bytes());
    key[8..16].copy_from_slice(&serial.to_le_bytes());
    key[24..].copy_from_slice(b"wr1-slot");
    Key(key)
}

impl<T> Slot<T> {
    /// An empty slot named `name`.
    #[expect(clippy::disallowed_types, reason = "makes the locks its fields allow")]
    pub fn empty(name: SlotName) -> Slot<T> {
        Slot {
            name,
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            writing: blaze_core::lock::Lock::new("query.slot.writing", ()),
            full: AtomicBool::new(false),
            claim: AtomicU8::new(UNCLAIMED),
            claimer: AtomicU64::new(0),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// A slot named `name` that holds `value`: what an input write that changes a filled slot
    /// sets, opening a view as every write does.
    pub fn full(name: SlotName, value: T) -> Slot<T> {
        let slot = Slot::empty(name);
        // SAFETY: the slot is new: nothing reads `value` before `full` is stored.
        unsafe { (*slot.value.get()).write(value) };
        slot.full.store(true, Ordering::Release);
        slot
    }

    pub fn name(&self) -> &SlotName {
        &self.name
    }

    /// Whether it holds its value, recording no read: for a scheduler deciding what is ready,
    /// never for a query, which must not see a slot empty (it reads with [`Slot::get`]).
    pub fn is_full(&self) -> bool {
        self.full.load(Ordering::Acquire)
    }

    fn filled(&self) -> Option<&T> {
        // Acquire: the value written before `full` was stored.
        // SAFETY: `full` says `value` is written, and it changes no more while `&self` lives.
        self.full.load(Ordering::Acquire).then(|| unsafe { (*self.value.get()).assume_init_ref() })
    }

    /// Its value, waiting while the task that claimed it runs: the wait suspends this task, and
    /// gives way ([`Cancelled::WouldWaitOnItself`], unwinding) where it would close a cycle, as a
    /// query's wait for another's flight does, and where the claim was given back for its
    /// filler to run again ([`SlotClaim::give_back`]). A slot no task claimed, one its filler
    /// gave up, or one read by the task that holds its claim is an error naming it.
    pub fn get<D: Database + ?Sized>(&self, db: &D) -> Result<&T, SlotError> {
        if let Some(value) = self.filled() {
            return Ok(value);
        }
        // Acquire: the claim's state, stored with release once its flight is held. A slot no
        // task claimed is told so here, before the flight: a read that ran the flight's empty
        // "run" could hold it just as its claimer takes it, and fail that claim with `Busy`.
        match self.claim.load(Ordering::Acquire) {
            UNCLAIMED => return Err(SlotError::Unclaimed { slot: self.name.clone() }), // alloc: clone: an error's name, once
            ABANDONED => return self.after_wait(),
            GIVEN_BACK => Self::give_way(),
            _ if self.claimer.load(Ordering::Relaxed) == task() => {
                return Err(SlotError::ReadByItsFiller { slot: self.name.clone() }); // alloc: clone: an error's name, once
            }
            _ => {}
        }
        let storage = db.storage();
        let barrier = local::barrier();
        let named = |it: &mut String| {
            use std::fmt::Write as _;
            _ = write!(it, "{}", self.name);
        };
        // A task holds the slot's flight from its claim to its fill: the wait ends with it. Its
        // holder let go of it meanwhile: this "run" ends at once, and the slot says why.
        let got = storage.flights.get_or_run_named(key(self.serial), named, || {
            let _lifted = barrier.lift();
            Ok(())
        });
        drop(barrier);
        if got.is_err() {
            Cancelled::WouldWaitOnItself { asked: None }.throw();
        }
        self.after_wait()
    }

    /// What a read finds once no task holds the slot's flight for it.
    fn after_wait(&self) -> Result<&T, SlotError> {
        if let Some(value) = self.filled() {
            return Ok(value);
        }
        match self.claim.load(Ordering::Acquire) {
            // alloc: clone: an error's name, once
            ABANDONED => Err(SlotError::Abandoned { slot: self.name.clone() }),
            // Given back while this read waited, or claimed again for the rerun since: the value
            // comes from the rerun (after the filler's loop, or sooner in a task that claimed it
            // anew), which this read's own loop may be what waits for.
            GIVEN_BACK | CLAIMED => Self::give_way(),
            // alloc: clone: as above
            _ => Err(SlotError::Unclaimed { slot: self.name.clone() }),
        }
    }

    /// Gives way: the read waits for a filler that will run again, at the latest once its loop
    /// is done, and this read may be part of that loop: a cycle through the loop's owner.
    fn give_way() -> ! {
        _ = blaze_graph::pump::give_way();
        Cancelled::WouldWaitOnItself { asked: None }.throw()
    }

    /// Claims the slot for this task, which fills it: until the claim fills it or is dropped,
    /// a reader waits on this task, its wait's span naming the slot with this task as owner.
    ///
    /// # Errors
    ///
    /// Each names the slot: [`SlotError::Full`] once filled, [`SlotError::Busy`] while another
    /// task holds it, [`SlotError::ClaimedAgain`] when this task holds it already.
    pub fn claim<'s, D: Database + ?Sized>(
        &'s self,
        db: &'s D,
    ) -> Result<SlotClaim<'s, T>, SlotError> {
        if self.is_full() {
            return Err(SlotError::Full { slot: self.name.clone() }); // alloc: clone: an error's name, once
        }
        let named = |it: &mut String| {
            use std::fmt::Write as _;
            _ = write!(it, "{}", self.name);
        };
        let held = match db.storage().flights.hold_named(key(self.serial), named) {
            Ok(held) => held,
            // alloc: clone: an error's name, once
            Err(NotHeld::Busy) => return Err(SlotError::Busy { slot: self.name.clone() }),
            // alloc: clone: as above
            Err(NotHeld::ByThisTask) => {
                return Err(SlotError::ClaimedAgain { slot: self.name.clone() });
            }
        };
        self.claimer.store(task(), Ordering::Relaxed);
        // Release: the flight is held, and the claimer written, before a reader sees the claim.
        self.claim.store(CLAIMED, Ordering::Release);
        Ok(SlotClaim { slot: self, held: Some(held) })
    }
}

impl<T> Slot<T> {
    /// Marks the slot given up unfilled, unless a task holds its claim or it is full: what a
    /// maintainer whose pass failed does to every slot it left unfilled, so that a reader of
    /// it, one given back for a rerun the failed pass will never make or one never claimed,
    /// reads [`SlotError::Abandoned`] at once rather than giving way until some later pass
    /// fills it. Whether it marked it. A later claim (a later pass) claims it as any other.
    pub fn abandon(&self) -> bool {
        let mut now = self.claim.load(Ordering::Acquire);
        loop {
            if self.is_full() || !matches!(now, UNCLAIMED | GIVEN_BACK) {
                return false;
            }
            // Release, as a claim dropped unfilled stores it: a reader that loads it with
            // acquire reads it before the flight, and nothing holds the flight to wake.
            match self.claim.compare_exchange_weak(
                now,
                ABANDONED,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(was) => now = was,
            }
        }
    }
}

impl<T: PartialEq> Slot<T> {
    /// Fills the slot with `value` by `&`: see the module. A task that claimed it fills it
    /// through its claim ([`SlotClaim::fill`]), which wakes its readers.
    pub fn fill(&self, value: T) -> Result<Filled, SlotRefused> {
        let _writing = self.writing.lock();
        if let Some(held) = self.filled() {
            return match *held == value {
                true => Ok(Filled::Again),
                false => Err(SlotRefused { slot: self.name.clone() }), // alloc: clone: an error's name, once
            };
        }
        // SAFETY: under `writing`, and `full` is unset: no one wrote `value` or reads it.
        unsafe { (*self.value.get()).write(value) };
        // Release: the value is written before a reader sees the slot full.
        self.full.store(true, Ordering::Release);
        Ok(Filled::First)
    }
}

impl<T> Drop for Slot<T> {
    fn drop(&mut self) {
        if *self.full.get_mut() {
            // SAFETY: `full` says `value` is written; `&mut` says no one reads it.
            unsafe { self.value.get_mut().assume_init_drop() };
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Slot<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot").field("name", &self.name).field("value", &self.filled()).finish()
    }
}

/// A task's claim of a [`Slot`]: dropped unfilled, its readers stop waiting and read
/// [`SlotError::Abandoned`]; given back ([`SlotClaim::give_back`]), they give way.
pub struct SlotClaim<'s, T> {
    slot: &'s Slot<T>,
    held: Option<Held<'s, ()>>,
}

impl<T> SlotClaim<'_, T> {
    /// Lets go of the claim unfilled for its filler to run again and claim it anew: what a
    /// filler whose wait gave way does as it unwinds. A reader meanwhile gives way, and reads
    /// the value once it runs again after the filler has.
    pub fn give_back(mut self) {
        self.let_go(GIVEN_BACK);
    }

    /// The claim's state set to `to` before its flight is let go of, so a reader the flight
    /// wakes finds it.
    fn let_go(&mut self, to: u8) {
        if let Some(held) = self.held.take() {
            self.slot.claimer.store(0, Ordering::Relaxed);
            self.slot.claim.store(to, Ordering::Release);
            drop(held);
        }
    }
}

impl<T> Drop for SlotClaim<'_, T> {
    fn drop(&mut self) {
        self.let_go(ABANDONED);
    }
}

impl<T: PartialEq> SlotClaim<'_, T> {
    /// Fills the slot ([`Slot::fill`]) and lets its readers go on.
    pub fn fill(mut self, value: T) -> Result<Filled, SlotRefused> {
        let filled = self.slot.fill(value);
        if let Some(held) = self.held.take() {
            held.end(&());
        }
        filled
    }
}

#[cfg(test)]
mod tests;
