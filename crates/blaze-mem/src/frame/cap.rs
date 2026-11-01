//! The cap on task frames (docs/v2/plan/lanes/core.md, "Hard backstop"): a shared limit their
//! growth is reserved from, so memory admission's backstop holds for frame memory too.
//!
//! Bytes are reserved before they are taken and given back when they are. Past `spill_at`,
//! its [`Spills`] hears the bytes over it, once each time it is passed; a reserve that would
//! pass `limit` waits until enough is given back. A holder is a thread holding bytes of the
//! cap; when every holder is itself waiting, nothing can be given back, and the newest of those
//! waits is cancelled: its task unwinds with [`Cancelled`], which gives its bytes back, and the
//! scheduler runs it again. A thread that holds nothing is never the one cancelled. A waiter is
//! let in as soon as its bytes fit, whoever waited longer.
//!
//! A thread holding bytes reserves with one compare-and-swap on the cap's total and gives back
//! with one subtraction; only a thread starting or ending its holding, a reserve that waits, and a
//! give-back while one waits take the lock. Task frames reserve only past the chunks a thread
//! keeps warm ([`super`]'s shelf) and for their spans' pages, so a warm task reserves nothing.
//!
//! A thread holds bytes of one cap at a time: [`set_cap`] takes effect on a thread once its
//! frames hold none of the cap before.

use std::{cell::Cell, fmt, marker::PhantomData, ptr};

use crate::sync::{
    Condvar, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// What a task unwinds with when the cap cancelled its frame's growth: every holder of the cap
/// was waiting, and its wait was the newest. Its frame's memory is given back as it unwinds; the
/// scheduler catches it and runs the task again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// A limit on the bytes the process's task frames hold, past what each thread keeps warm.
#[derive(Clone, Copy)]
pub struct Cap(&'static Shared);

/// What a [`Cap`] says each time what it holds passes `spill_at`: the bytes over it, for a
/// spiller that moves cold values out of memory. The reserve that passed it only sends.
pub struct Spills(crossbeam_channel::Receiver<u64>);

impl Spills {
    /// The next pass of `spill_at`, waiting for it.
    pub fn next(&self) -> Option<u64> {
        self.0.recv().ok()
    }

    /// The passes not yet taken, in order, without waiting.
    pub fn ready(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.try_iter()
    }
}

#[cfg_attr(
    not(blaze_check),
    expect(
        clippy::disallowed_types,
        reason = "std's `Mutex` and `Condvar` for the backstop's wait (a reserve past `limit` blocks until bytes are given back), a cold lock inside the allocator (brief-locks.md's exception): parking_lot's parking table would allocate from within an allocation; holding threads reserve and give back with atomics"
    )
)]
struct Shared {
    limit: u64,
    spill_at: u64,
    held: AtomicU64,
    /// Whether `held` is over `spill_at` since the last pass was sent.
    above: AtomicBool,
    /// Reserves registered as waiting, so a give-back knows to wake them.
    waiting: AtomicU64,
    spills: crossbeam_channel::Sender<u64>,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    /// Threads holding bytes of the cap.
    holders: u32,
    waiters: Vec<Waiter>,
    next_wait: u64,
}

struct Waiter {
    /// Its order of coming: the newest is the greatest.
    order: u64,
    bytes: u64,
    /// Whether its thread holds bytes of the cap while it waits.
    holds: bool,
    cancelled: bool,
}

/// Why a reserve was refused.
#[derive(Clone, Copy)]
pub(crate) enum Refused {
    /// Every holder waited and this wait was the newest.
    Cancelled,
    /// More than the whole limit: no wait would end.
    TooBig { limit: u64 },
}

// Under the model checker each model thread has its own: model threads share an OS thread,
// whose `std` value would carry what one execution's thread held into the next's.
#[cfg(blaze_check)]
use blaze_check::thread_local;

thread_local! {
    /// The cap the calling thread holds bytes of, and how many.
    static HOLDING: Cell<(Option<&'static Shared>, u64)> = const { Cell::new((None, 0)) };
}

/// The cap task frames reserve from; null when none is set. `std`'s atomic even under the model
/// checker: it is process-wide, outside any model, and no model installs a cap.
static INSTALLED: std::sync::atomic::AtomicPtr<Shared> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());

impl Cap {
    /// A cap of `limit` bytes, and what hears each time it passes `spill_at`.
    pub fn new(limit: u64, spill_at: u64) -> (Cap, Spills) {
        let (spills, heard) = crossbeam_channel::unbounded();
        let state = State {
            holders: 0,
            // alloc: once per cap, room for the waiters of a machine's workers
            waiters: Vec::with_capacity(256),
            next_wait: 0,
        };
        // alloc: once per cap; it lives as long as the process, since a thread's holding names it
        #[cfg_attr(
            not(blaze_check),
            expect(
                clippy::disallowed_types,
                reason = "`Shared::state` and `changed`: a cold lock inside the allocator (brief-locks.md's exception): parking_lot's parking table would allocate from within an allocation"
            )
        )]
        let shared: &'static Shared = Box::leak(Box::new(Shared {
            limit,
            spill_at,
            held: AtomicU64::new(0),
            above: AtomicBool::new(false),
            waiting: AtomicU64::new(0),
            spills,
            state: Mutex::new(state),
            changed: Condvar::new(),
        }));
        (Cap(shared), Spills(heard))
    }

    pub fn limit(&self) -> u64 {
        self.0.limit
    }

    /// The bytes reserved and not given back.
    pub fn held(&self) -> u64 {
        self.0.held.load(Ordering::Relaxed)
    }

    /// How many reserves wait now.
    pub fn waiting(&self) -> usize {
        self.0.lock().waiters.iter().filter(|it| !it.cancelled).count()
    }

    /// `bytes` of the cap for the calling thread, waiting while they would pass its limit, or
    /// [`Cancelled`] when every holder waits. Given back when the [`Grant`] drops, on this thread.
    ///
    /// # Panics
    ///
    /// When `bytes` alone are more than the limit (no wait would end), or when the thread holds
    /// bytes of another cap.
    pub fn reserve(&self, bytes: u64) -> Result<Grant, Cancelled> {
        match reserve(self.0, bytes) {
            Ok(()) => Ok(Grant { cap: self.0, bytes, _thread: PhantomData }),
            Err(Refused::Cancelled) => Err(Cancelled),
            Err(Refused::TooBig { limit }) => {
                panic!("a reserve of {bytes} bytes, more than the cap of {limit}")
            }
        }
    }
}

impl fmt::Debug for Cap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cap")
            .field("limit", &self.0.limit)
            .field("spill_at", &self.0.spill_at)
            .field("held", &self.held())
            .finish()
    }
}

/// Bytes of a [`Cap`] the thread that took them holds, given back when dropped there.
pub struct Grant {
    cap: &'static Shared,
    bytes: u64,
    /// Given back on the thread that holds it: a holder is a thread.
    _thread: PhantomData<*const ()>,
}

impl Grant {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant").field("bytes", &self.bytes).finish()
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        release(self.cap, self.bytes);
    }
}

/// Sets the cap every task frame grows under from now on, or none; a thread whose frames hold
/// bytes of the cap before keeps reserving from that one until they hold none.
pub fn set_cap(cap: Option<Cap>) {
    let shared = cap.map_or(ptr::null_mut(), |cap| ptr::from_ref(cap.0).cast_mut());
    INSTALLED.store(shared, Ordering::Release);
}

/// The cap task frames grow under, if one is set.
pub fn cap() -> Option<Cap> {
    installed().map(Cap)
}

fn installed() -> Option<&'static Shared> {
    let shared = INSTALLED.load(Ordering::Acquire);
    // SAFETY: `INSTALLED` holds null or a `Shared` that `Cap::new` leaked, which is never freed.
    unsafe { shared.as_ref() }
}

/// Bytes the calling thread holds of a cap.
pub(crate) fn thread_holding() -> u64 {
    HOLDING.try_with(|it| it.get().1).unwrap_or(0)
}

/// Reserves `bytes` for the calling thread's task frames: from the cap the thread holds bytes
/// of, else the one set. Whether a cap took them (`false`: no cap is set, nothing to give back).
pub(crate) fn frame_reserve(bytes: usize) -> Result<bool, Refused> {
    let holding = HOLDING.try_with(Cell::get).unwrap_or((None, 0));
    let shared = match holding {
        (Some(shared), held) if held > 0 => shared,
        _ => match installed() {
            Some(shared) => shared,
            None => return Ok(false),
        },
    };
    reserve(shared, bytes as u64).map(|()| true)
}

/// Gives back `bytes` the calling thread's task frames reserved with [`frame_reserve`].
pub(crate) fn frame_release(bytes: usize) {
    match HOLDING.try_with(Cell::get) {
        Ok((Some(shared), held)) if held >= bytes as u64 => release(shared, bytes as u64),
        // A thread being torn down has lost its count: its bytes stay counted.
        Err(_) => {}
        Ok(_) => unreachable!("a frame gave back {bytes} bytes of a cap its thread does not hold"),
    }
}

impl Shared {
    #[cfg_attr(
        not(blaze_check),
        expect(
            clippy::disallowed_types,
            reason = "the guard of `Shared::state`: a cold lock inside the allocator (brief-locks.md's exception): parking_lot's parking table would allocate from within an allocation"
        )
    )]
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `bytes` if they fit: the total after.
    fn try_add(&self, bytes: u64) -> Option<u64> {
        let limit = self.limit;
        self.held
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |held| {
                held.checked_add(bytes).filter(|&after| after <= limit)
            })
            .ok()
            .map(|before| before + bytes)
    }

    fn passed(&self, after: u64) {
        if after > self.spill_at && !self.above.swap(true, Ordering::AcqRel) {
            // A cap whose `Spills` was dropped has no spiller: the send fails, and that changes
            // nothing.
            // alloc: a channel block now and then, once per pass of `spill_at`
            _ = self.spills.send(after - self.spill_at);
        }
    }
}

impl State {
    /// When every holder waits and no waiter fits, cancels the newest waiter that holds bytes.
    /// Whether it cancelled one.
    fn unstick(&mut self, shared: &Shared) -> bool {
        if self.holders == 0 {
            return false;
        }
        let held = shared.held.load(Ordering::SeqCst);
        let live = || self.waiters.iter().filter(|it| !it.cancelled);
        if live().any(|it| held.saturating_add(it.bytes) <= shared.limit) {
            return false;
        }
        let waiting_holders = live().filter(|it| it.holds).count();
        if waiting_holders < self.holders as usize {
            return false;
        }
        let victim = self
            .waiters
            .iter_mut()
            .filter(|it| !it.cancelled && it.holds)
            .max_by_key(|it| it.order);
        match victim {
            Some(victim) => {
                victim.cancelled = true;
                true
            }
            None => false,
        }
    }

    fn remove(&mut self, order: u64, shared: &Shared) {
        if let Some(at) = self.waiters.iter().position(|it| it.order == order) {
            self.waiters.swap_remove(at);
            shared.waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn reserve(shared: &'static Shared, bytes: u64) -> Result<(), Refused> {
    if bytes > shared.limit {
        return Err(Refused::TooBig { limit: shared.limit });
    }
    let (holding, mine) = HOLDING.try_with(Cell::get).unwrap_or((None, 0));
    if let Some(other) = holding
        && mine > 0
        && !ptr::eq(other, shared)
    {
        panic!("a thread holds bytes of two caps at once");
    }
    let holds = mine > 0;
    if holds && let Some(after) = shared.try_add(bytes) {
        shared.passed(after);
        HOLDING.set((Some(shared), mine + bytes));
        return Ok(());
    }
    // A thread starting to hold, or a reserve over the limit: under the lock, so the count of
    // holders and of the holders waiting agree.
    let mut state = shared.lock();
    let mut waiting: Option<u64> = None;
    loop {
        if let Some(order) = waiting
            && state.waiters.iter().any(|it| it.order == order && it.cancelled)
        {
            state.remove(order, shared);
            drop(state);
            shared.changed.notify_all();
            return Err(Refused::Cancelled);
        }
        if let Some(after) = shared.try_add(bytes) {
            if let Some(order) = waiting {
                state.remove(order, shared);
            }
            if !holds {
                state.holders += 1;
            }
            drop(state);
            shared.passed(after);
            HOLDING.set((Some(shared), mine + bytes));
            return Ok(());
        }
        if waiting.is_none() {
            let order = state.next_wait;
            state.next_wait += 1;
            // alloc: only past the room made with the cap, a waiter more than a machine has
            state.waiters.push(Waiter { order, bytes, holds, cancelled: false });
            shared.waiting.fetch_add(1, Ordering::SeqCst);
            waiting = Some(order);
            // Registered before looking again, so a give-back from now on wakes it.
            continue;
        }
        if state.unstick(shared) {
            shared.changed.notify_all();
            continue;
        }
        state = shared.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
    }
}

fn release(shared: &'static Shared, bytes: u64) {
    let (_, mine) = HOLDING.try_with(Cell::get).unwrap_or((Some(shared), bytes));
    let left = mine.saturating_sub(bytes);
    _ = HOLDING.try_with(|it| it.set((if left == 0 { None } else { Some(shared) }, left)));
    let after = shared.held.fetch_sub(bytes, Ordering::SeqCst) - bytes;
    if after <= shared.spill_at {
        shared.above.store(false, Ordering::Release);
    }
    if left == 0 {
        let mut state = shared.lock();
        state.holders = state.holders.saturating_sub(1);
        let wake = !state.waiters.is_empty();
        if wake {
            state.unstick(shared);
        }
        drop(state);
        if wake {
            shared.changed.notify_all();
        }
    } else if shared.waiting.load(Ordering::SeqCst) > 0 {
        let mut state = shared.lock();
        state.unstick(shared);
        drop(state);
        shared.changed.notify_all();
    }
}
