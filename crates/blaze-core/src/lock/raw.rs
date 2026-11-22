//! The bottom of blaze's locks: a mutex, a reader-writer lock and a condition, each on one or
//! two 32-bit words that threads wait on in the kernel ([`crate::wait::word_wait`], a futex on
//! Linux and the wait on an address on macOS). [`super::Lock`], [`super::ReadLock`] and
//! [`super::Signal`] are these with a name and the profile's records; nothing else in the tree
//! takes them (rule standard-structures).
//!
//! - [`Mutex`]: one word, 0 free, 1 held, 2 held with waiters (Drepper's "Futexes are tricky",
//!   mutex 3, as std's futex mutex). An uncontended acquire is one compare-and-swap (acquire),
//!   a release one swap (release) and, only when the word said 2, one wake of one waiter. A
//!   contended acquire spins a few rounds ([`blaze_spin::SpinWait::pause`]) while the holder
//!   may be about to let go, then marks the word 2 and sleeps on it.
//! - [`RwLock`]: one word of readers and two waiting bits, and a second word writers sleep on
//!   (std's futex reader-writer lock): readers count in the low 30 bits, `WRITE_LOCKED` is all
//!   of them set; a waiting writer stops new readers, so writers are not starved.
//! - [`Cond`]: a sequence word. A wait reads it under the lock, lets the lock go and sleeps
//!   while it still holds that value; a notify adds one to it and wakes one waiter or all.
//!
//! Orderings, in one line: every acquire that takes a lock is an `Acquire` read-modify-write
//! and every release a `Release` one, so a holder's writes happen before the next holder's
//! reads; the waiting bits and the condition's sequence are `Relaxed`, ordered by the lock's
//! own word and by the kernel's compare-and-block, which reads the word in the same step as it
//! sleeps.
//!
//! Under `--cfg blaze_check` the atomics are the model checker's and a word's wait is a model
//! mutex and condition variable beside it (the kernel's compare-and-block, as the model sees
//! it), so every interleaving of these protocols is explored; `raw/tests.rs` checks each with a
//! planted bug ([`Plant`]) the model must catch.

#![allow(
    unsafe_code,
    reason = "a lock's data is reached through its word: each `unsafe` says what the word orders"
)]

use std::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    time::Duration,
};

#[cfg(not(blaze_check))]
use blaze_spin::SpinWait;

// std's atomics, never `crate::atomic`'s: in blaze-core's tests those are the scheduled ones,
// and a lock is a real lock there (the scheduler's own baton is one: scheduling its word would
// recurse). Under `--cfg blaze_check`, the model checker's.
#[cfg(not(blaze_check))]
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicU32, Ordering};

#[cfg(test)]
mod tests;

/// A safeguard of the protocols here that a model test removes, to see that the model catches
/// the removal. Production code passes [`Plant::None`], a constant the branch folds away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A [`Mutex`] release that finds the word contended (2) wakes no one: the waiter asleep
    /// on it sleeps on. (A woken waiter taking the word as 1, Drepper's other lost wakeup, is
    /// no model control: the next waiter's wait for a 2 then returns at once, so it spins
    /// rather than sleeps, and the model reports the spin before any lost waiter.)
    #[cfg(test)]
    ReleaseUnwoken,
    /// A [`RwLock`]'s write release wakes no one when only readers wait: they sleep on.
    #[cfg(test)]
    ReadersUnwoken,
    /// A [`Cond`] notify wakes without moving the sequence: a waiter that read it and let the
    /// lock go, but is not yet asleep, sleeps through the notify.
    #[cfg(test)]
    SequenceUnmoved,
}

// --- Word ---------------------------------------------------------------------------------------

/// A word as the kernel's wait takes it: `crate::atomic`'s type, which is std's outside
/// blaze-core's tests.
#[cfg(all(not(blaze_check), not(test), any(target_os = "linux", target_os = "macos")))]
fn kernel_word(word: &AtomicU32) -> &crate::atomic::AtomicU32 {
    word
}

/// In blaze-core's tests `crate::atomic`'s type is the scheduled one, `repr(transparent)` over
/// std's: the same word, which the kernel compares and wakes by address alone; no operation of
/// the scheduled type is made on it.
#[cfg(all(not(blaze_check), test, any(target_os = "linux", target_os = "macos")))]
fn kernel_word(word: &AtomicU32) -> &crate::atomic::AtomicU32 {
    // SAFETY: `scheduled::AtomicU32` is `#[repr(transparent)]` over `std`'s `AtomicU32`, so the
    // reference is to the same bytes with the same layout, for the same lifetime.
    unsafe { &*std::ptr::from_ref(word).cast::<crate::atomic::AtomicU32>() }
}

/// A word threads sleep on while it holds a value, and the wake of one of them or of all.
pub(crate) struct Word {
    pub(crate) value: AtomicU32,
    /// The model's compare-and-block: a model lock the compare is made under and a model
    /// condition the sleep is.
    #[cfg(blaze_check)]
    model: (blaze_check::sync::Mutex<()>, blaze_check::sync::Condvar),
}

/// What a word's errors call the wait.
const WHAT: &str = "a lock's word";

impl Word {
    pub(crate) const fn new(value: u32) -> Word {
        Word {
            value: AtomicU32::new(value),
            #[cfg(blaze_check)]
            model: (blaze_check::sync::Mutex::new(()), blaze_check::sync::Condvar::new()),
        }
    }

    /// Sleeps while the word holds `expected`, for at most `within` (no limit for `None`):
    /// whether `within` passed. It may return early with nothing (the caller looks again).
    /// A failure of the kernel's wait is a panic naming it: a lock has no error to return, and
    /// the only ones are a bad address or a clock that does not move (rule no-fallback).
    #[cfg(all(not(blaze_check), any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn wait(&self, expected: u32, within: Option<Duration>) -> bool {
        use crate::wait::{Clock as _, Deadline, Monotonic, Waited};
        let clock = Monotonic::new();
        let deadline = within.map(|it| Deadline::after(clock.now(), it));
        match crate::wait::word_wait(WHAT, kernel_word(&self.value), expected, deadline, &clock) {
            Ok(Waited::Ready(())) => false,
            Ok(Waited::Deadline) => true,
            Err(error) => panic!("{error} (rule no-fallback: a lock's wait has no other way)"),
        }
    }

    #[cfg(all(not(blaze_check), any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn wake(&self, all: bool) {
        crate::wait::word_wake(kernel_word(&self.value), all);
    }

    /// Windows and the rest are out of scope (AGENTS.md, "Not now"): there a wait is a yield,
    /// an early return the caller's loop allows, and a wake is nothing.
    #[cfg(all(not(blaze_check), not(any(target_os = "linux", target_os = "macos"))))]
    #[expect(clippy::disallowed_methods, reason = "no kernel wait on a word is built here")]
    pub(crate) fn wait(&self, expected: u32, within: Option<Duration>) -> bool {
        _ = (expected, within);
        std::thread::yield_now();
        false
    }

    #[cfg(all(not(blaze_check), not(any(target_os = "linux", target_os = "macos"))))]
    pub(crate) fn wake(&self, all: bool) {
        _ = all;
    }

    #[cfg(blaze_check)]
    pub(crate) fn wait(&self, expected: u32, within: Option<Duration>) -> bool {
        use std::sync::PoisonError;
        let (lock, cond) = &self.model;
        let held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        // The kernel's compare, in the step that blocks: a wake after it finds the sleeper.
        if self.value.load(Ordering::Relaxed) != expected {
            return false;
        }
        match within {
            None => {
                drop(cond.wait(held).unwrap_or_else(PoisonError::into_inner));
                false
            }
            Some(within) => {
                let (held, timed) =
                    cond.wait_timeout(held, within).unwrap_or_else(PoisonError::into_inner);
                drop(held);
                timed.timed_out()
            }
        }
    }

    #[cfg(blaze_check)]
    pub(crate) fn wake(&self, all: bool) {
        use std::sync::PoisonError;
        let (lock, cond) = &self.model;
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        match all {
            true => cond.notify_all(),
            false => cond.notify_one(),
        }
    }
}

/// Spins while `busy` says the word is held by a thread likely to let go soon, a few doubling
/// rounds and never a yield: the word as last read.
#[cfg(not(blaze_check))]
#[inline]
fn spin_while(word: &AtomicU32, busy: impl Fn(u32) -> bool) -> u32 {
    let mut spin = SpinWait::new();
    loop {
        let state = word.load(Ordering::Relaxed);
        if !busy(state) || !spin.pause() {
            return state;
        }
    }
}

/// In a model the spin is one look: it only reads, so it changes no outcome, and each of its
/// rounds would be a schedule point the model explores for nothing.
#[cfg(blaze_check)]
fn spin_while(word: &AtomicU32, busy: impl Fn(u32) -> bool) -> u32 {
    _ = busy;
    word.load(Ordering::Relaxed)
}

/// The time left of `within` once `spent` has passed: `None` for no limit.
fn left(within: Option<Duration>, spent: Duration) -> Option<Duration> {
    within.map(|it| it.saturating_sub(spent))
}

// --- Mutex --------------------------------------------------------------------------------------

const FREE: u32 = 0;
const HELD: u32 = 1;
/// Held, and a thread sleeps (or is about to) on the word: the release must wake one.
const CONTENDED: u32 = 2;

/// The mutex protocol on one word, without data.
pub(crate) struct RawMutex {
    word: Word,
}

impl RawMutex {
    pub(crate) const fn new() -> RawMutex {
        RawMutex { word: Word::new(FREE) }
    }

    #[inline]
    pub(crate) fn try_lock(&self) -> bool {
        self.word.value.compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    #[inline]
    pub(crate) fn lock(&self) {
        if !self.try_lock() {
            self.lock_contended(None);
        }
    }

    /// The lock within `within`: whether it was taken.
    pub(crate) fn lock_within(&self, within: Duration) -> bool {
        self.try_lock() || self.lock_contended(Some(within))
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.word.value.load(Ordering::Relaxed) != FREE
    }

    /// The slow path: spin while the holder may be about to let go, then mark the word
    /// contended and sleep until it is free. Whether it was taken before `within` passed.
    #[cold]
    #[inline(never)]
    pub(crate) fn lock_contended(&self, within: Option<Duration>) -> bool {
        let started = within.map(|_| Clock::start());
        let mut state = spin_while(&self.word.value, |it| it == HELD);
        if state == FREE {
            match self.word.value.compare_exchange(
                FREE,
                HELD,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => state = now,
            }
        }
        // Taken from here on as contended: some other thread may still sleep on the word, and
        // only a 2 makes this thread's release wake it.
        loop {
            if state != CONTENDED && self.word.value.swap(CONTENDED, Ordering::Acquire) == FREE {
                return true;
            }
            let spent = started.as_ref().map_or(Duration::ZERO, Clock::spent);
            let rest = left(within, spent);
            if rest == Some(Duration::ZERO) {
                return false;
            }
            // wakes: `unlock`, which wakes one sleeper whenever the word said 2
            self.word.wait(CONTENDED, rest);
            state = spin_while(&self.word.value, |it| it == HELD);
        }
    }

    /// Lets the lock go.
    ///
    /// # Safety
    ///
    /// The calling thread holds it (a `lock`, `try_lock` or `lock_within` that took it, not
    /// yet let go).
    #[inline]
    pub(crate) unsafe fn unlock(&self) {
        // SAFETY: the caller's.
        unsafe { self.unlock_with(Plant::None) };
    }

    /// [`RawMutex::unlock`], with a safeguard removed for a model test.
    ///
    /// # Safety
    ///
    /// As [`RawMutex::unlock`].
    #[inline]
    pub(crate) unsafe fn unlock_with(&self, plant: Plant) {
        #[cfg(test)]
        let wakes = plant != Plant::ReleaseUnwoken;
        #[cfg(not(test))]
        let wakes = {
            _ = plant;
            true
        };
        if self.word.value.swap(FREE, Ordering::Release) == CONTENDED && wakes {
            self.word.wake(false);
        }
    }
}

/// The clock of a wait with a limit, read only on a slow path that has one.
struct Clock(crate::wait::Monotonic);

impl Clock {
    fn start() -> Clock {
        Clock(crate::wait::Monotonic::new())
    }

    fn spent(&self) -> Duration {
        use crate::wait::Clock as _;
        self.0.now()
    }
}

/// A value under a [`RawMutex`].
pub(crate) struct Mutex<T: ?Sized> {
    raw: RawMutex,
    data: UnsafeCell<T>,
}

// SAFETY: the lock hands its value to one thread at a time, as std's `Mutex`: sending the lock
// sends the value, and sharing it lets each thread in turn reach the value mutably.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

/// The hold of a [`Mutex`], let go when it drops.
#[must_use = "a lock is let go when its guard drops"]
pub(crate) struct MutexGuard<'a, T: ?Sized> {
    lock: &'a Mutex<T>,
    /// Not `Send`: the thread that took the lock lets it go.
    _thread: std::marker::PhantomData<*const ()>,
}

// SAFETY: a shared guard gives `&T` only.
unsafe impl<T: ?Sized + Sync> Sync for MutexGuard<'_, T> {}

impl<T> Mutex<T> {
    pub(crate) const fn new(value: T) -> Mutex<T> {
        Mutex { raw: RawMutex::new(), data: UnsafeCell::new(value) }
    }

    pub(crate) fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    fn guard(&self) -> MutexGuard<'_, T> {
        MutexGuard { lock: self, _thread: std::marker::PhantomData }
    }

    #[inline]
    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.raw.lock();
        self.guard()
    }

    #[inline]
    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.raw.try_lock().then(|| self.guard())
    }

    pub(crate) fn try_lock_for(&self, within: Duration) -> Option<MutexGuard<'_, T>> {
        self.raw.lock_within(within).then(|| self.guard())
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.raw.is_locked()
    }

    pub(crate) fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    pub(crate) fn data_ptr(&self) -> *mut T {
        self.data.get()
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this guard is the hold its thread took.
        unsafe { self.lock.raw.unlock() };
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: the guard holds the lock, so no other thread reaches the value.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` is the one reference through this guard.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

// --- RwLock -------------------------------------------------------------------------------------

/// One reader.
const READ_LOCKED: u32 = 1;
/// The readers' count; all of it set is a writer.
const MASK: u32 = (1 << 30) - 1;
const WRITE_LOCKED: u32 = MASK;
const MAX_READERS: u32 = MASK - 1;
const READERS_WAITING: u32 = 1 << 30;
const WRITERS_WAITING: u32 = 1 << 31;

#[inline]
fn is_unlocked(state: u32) -> bool {
    state & MASK == 0
}

#[inline]
fn is_write_locked(state: u32) -> bool {
    state & MASK == WRITE_LOCKED
}

#[inline]
fn has_readers_waiting(state: u32) -> bool {
    state & READERS_WAITING != 0
}

#[inline]
fn has_writers_waiting(state: u32) -> bool {
    state & WRITERS_WAITING != 0
}

/// A reader may take it: not write-locked, room for one more, and nobody waiting (a waiting
/// writer stops new readers, so writers are not starved by a stream of them).
#[inline]
fn is_read_lockable(state: u32) -> bool {
    state & MASK < MAX_READERS && !has_readers_waiting(state) && !has_writers_waiting(state)
}

/// The reader-writer protocol on two words, without data.
pub(crate) struct RawRwLock {
    /// Readers, or `WRITE_LOCKED`, and the two waiting bits. Readers sleep on it.
    state: Word,
    /// Bumped each time a writer is woken. Writers sleep on it.
    writer_notify: Word,
}

impl RawRwLock {
    pub(crate) const fn new() -> RawRwLock {
        RawRwLock { state: Word::new(0), writer_notify: Word::new(0) }
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.state.value.load(Ordering::Relaxed) & MASK != 0
    }

    #[inline]
    pub(crate) fn try_read(&self) -> bool {
        let mut state = self.state.value.load(Ordering::Relaxed);
        while is_read_lockable(state) {
            match self.state.value.compare_exchange_weak(
                state,
                state + READ_LOCKED,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => state = now,
            }
        }
        false
    }

    #[inline]
    pub(crate) fn read(&self) {
        let state = self.state.value.load(Ordering::Relaxed);
        if !is_read_lockable(state)
            || self
                .state
                .value
                .compare_exchange_weak(state, state + READ_LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
        {
            let _ = self.read_contended(None);
        }
    }

    #[inline]
    pub(crate) fn read_within(&self, within: Duration) -> bool {
        let state = self.state.value.load(Ordering::Relaxed);
        if is_read_lockable(state)
            && self
                .state
                .value
                .compare_exchange_weak(state, state + READ_LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            return true;
        }
        self.read_contended(Some(within))
    }

    #[cold]
    #[inline(never)]
    fn read_contended(&self, within: Option<Duration>) -> bool {
        let start = within.map(|_| std::time::Instant::now());
        let mut state = self.spin_read();
        loop {
            if is_read_lockable(state) {
                match self.state.value.compare_exchange_weak(
                    state,
                    state + READ_LOCKED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(now) => {
                        state = now;
                        continue;
                    }
                }
            }
            let remaining = left(within, start.map_or(Duration::ZERO, |s| s.elapsed()));
            if remaining == Some(Duration::ZERO) {
                return false;
            }
            assert!(state & MASK != MAX_READERS, "a read lock with {MAX_READERS} readers");
            // Say a reader waits before sleeping, so a writer's release wakes it.
            if !has_readers_waiting(state)
                && let Err(now) = self.state.value.compare_exchange(
                    state,
                    state | READERS_WAITING,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
            {
                state = now;
                continue;
            }
            // wakes: `wake_writer_or_readers`, which wakes every reader once no writer waits
            self.state.wait(state | READERS_WAITING, remaining);
            state = self.spin_read();
        }
    }

    /// Lets a read go.
    ///
    /// # Safety
    ///
    /// The calling thread holds a read of it.
    #[inline]
    pub(crate) unsafe fn unlock_read(&self) {
        let state = self.state.value.fetch_sub(READ_LOCKED, Ordering::Release) - READ_LOCKED;
        // A reader waits on a read-locked lock only behind a waiting writer, so the last
        // reader out wakes the writer.
        if is_unlocked(state) && has_writers_waiting(state) {
            self.wake_writer_or_readers(state, Plant::None);
        }
    }

    #[inline]
    pub(crate) fn try_write(&self) -> bool {
        let mut state = self.state.value.load(Ordering::Relaxed);
        while is_unlocked(state) {
            match self.state.value.compare_exchange_weak(
                state,
                state + WRITE_LOCKED,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => state = now,
            }
        }
        false
    }

    #[inline]
    pub(crate) fn write(&self) {
        if self
            .state
            .value
            .compare_exchange_weak(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            let _ = self.write_contended(None);
        }
    }

    #[inline]
    pub(crate) fn write_within(&self, within: Duration) -> bool {
        if self
            .state
            .value
            .compare_exchange_weak(0, WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
        self.write_contended(Some(within))
    }

    #[cold]
    #[inline(never)]
    fn write_contended(&self, within: Option<Duration>) -> bool {
        let start = within.map(|_| std::time::Instant::now());
        let mut state = self.spin_write();
        // Once this writer has waited, others may wait too: it keeps the bit when it takes it.
        let mut other_writers_waiting = 0;
        loop {
            if is_unlocked(state) {
                match self.state.value.compare_exchange_weak(
                    state,
                    state | WRITE_LOCKED | other_writers_waiting,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(now) => {
                        state = now;
                        continue;
                    }
                }
            }
            let remaining = left(within, start.map_or(Duration::ZERO, |s| s.elapsed()));
            if remaining == Some(Duration::ZERO) {
                return false;
            }
            if !has_writers_waiting(state)
                && let Err(now) = self.state.value.compare_exchange(
                    state,
                    state | WRITERS_WAITING,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
            {
                state = now;
                continue;
            }
            other_writers_waiting = WRITERS_WAITING;
            // Read the notify word before looking at the state again: a wake after this look
            // changes the word, so the sleep below returns at once.
            let seq = self.writer_notify.value.load(Ordering::Acquire);
            state = self.state.value.load(Ordering::Relaxed);
            if is_unlocked(state) || !has_writers_waiting(state) {
                continue;
            }
            // wakes: `wake_writer`, from the release that saw the writers' bit
            self.writer_notify.wait(seq, remaining);
            state = self.spin_write();
        }
    }

    /// Lets a write go.
    ///
    /// # Safety
    ///
    /// The calling thread holds the write of it.
    #[inline]
    pub(crate) unsafe fn unlock_write(&self) {
        // SAFETY: the caller's.
        unsafe { self.unlock_write_with(Plant::None) };
    }

    /// [`RawRwLock::unlock_write`], with a safeguard removed for a model test.
    ///
    /// # Safety
    ///
    /// As [`RawRwLock::unlock_write`].
    #[inline]
    pub(crate) unsafe fn unlock_write_with(&self, plant: Plant) {
        let state = self.state.value.fetch_sub(WRITE_LOCKED, Ordering::Release) - WRITE_LOCKED;
        debug_assert!(is_unlocked(state), "a write let go with readers counted");
        if has_writers_waiting(state) || has_readers_waiting(state) {
            self.wake_writer_or_readers(state, plant);
        }
    }

    /// Wakes a writer, else every reader, of a lock just let go (`state`, unlocked).
    #[cold]
    fn wake_writer_or_readers(&self, mut state: u32, plant: Plant) {
        debug_assert!(is_unlocked(state));
        // Only writers wait: wake one.
        if state == WRITERS_WAITING {
            match self.state.value.compare_exchange(state, 0, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => {
                    self.wake_writer();
                    return;
                }
                Err(now) => state = now,
            }
        }
        // Both wait: a writer first, leaving the readers' bit for its release. The kernel does
        // not say here whether a writer slept, so the readers are woken too: a writer only
        // spinning sees the notify word move and does not sleep, and the readers it would have
        // left asleep would be lost. They wake, see the writer, and wait again behind it.
        if state == READERS_WAITING + WRITERS_WAITING {
            if self
                .state
                .value
                .compare_exchange(state, READERS_WAITING, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                // Taken since: its holder's release wakes who waits.
                return;
            }
            self.wake_writer();
            state = READERS_WAITING;
        }
        #[cfg(test)]
        if plant == Plant::ReadersUnwoken {
            return;
        }
        _ = plant;
        // Only readers wait: wake all of them.
        if state == READERS_WAITING
            && self
                .state
                .value
                .compare_exchange(state, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.state.wake(true);
        }
    }

    fn wake_writer(&self) {
        self.writer_notify.value.fetch_add(1, Ordering::Release);
        self.writer_notify.wake(false);
    }

    /// Spins while a writer holds it and nobody waits.
    fn spin_read(&self) -> u32 {
        spin_while(&self.state.value, |it| {
            is_write_locked(it) && !has_readers_waiting(it) && !has_writers_waiting(it)
        })
    }

    /// Spins while it is held and no writer waits.
    fn spin_write(&self) -> u32 {
        spin_while(&self.state.value, |it| !is_unlocked(it) && !has_writers_waiting(it))
    }
}

/// A value under a [`RawRwLock`].
pub(crate) struct RwLock<T: ?Sized> {
    raw: RawRwLock,
    data: UnsafeCell<T>,
}

// SAFETY: as std's `RwLock`: readers share `&T` across threads (`T: Sync`), a writer reaches
// the value mutably from any thread (`T: Send`).
unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

/// A shared read of a [`RwLock`].
#[must_use = "a lock is let go when its guard drops"]
pub(crate) struct RwLockReadGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    _thread: std::marker::PhantomData<*const ()>,
}

/// The write of a [`RwLock`].
#[must_use = "a lock is let go when its guard drops"]
pub(crate) struct RwLockWriteGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    _thread: std::marker::PhantomData<*const ()>,
}

// SAFETY: a shared guard gives `&T` only.
unsafe impl<T: ?Sized + Sync> Sync for RwLockReadGuard<'_, T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Sync> Sync for RwLockWriteGuard<'_, T> {}

impl<T> RwLock<T> {
    pub(crate) const fn new(value: T) -> RwLock<T> {
        RwLock { raw: RawRwLock::new(), data: UnsafeCell::new(value) }
    }

    pub(crate) fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> RwLock<T> {
    fn reading(&self) -> RwLockReadGuard<'_, T> {
        RwLockReadGuard { lock: self, _thread: std::marker::PhantomData }
    }

    fn writing(&self) -> RwLockWriteGuard<'_, T> {
        RwLockWriteGuard { lock: self, _thread: std::marker::PhantomData }
    }

    #[inline]
    pub(crate) fn read(&self) -> RwLockReadGuard<'_, T> {
        self.raw.read();
        self.reading()
    }

    #[inline]
    pub(crate) fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        self.raw.try_read().then(|| self.reading())
    }

    #[inline]
    pub(crate) fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.raw.write();
        self.writing()
    }

    #[inline]
    pub(crate) fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        self.raw.try_write().then(|| self.writing())
    }

    #[inline]
    pub(crate) fn try_read_for(&self, within: Duration) -> Option<RwLockReadGuard<'_, T>> {
        self.raw.read_within(within).then(|| self.reading())
    }

    #[inline]
    pub(crate) fn try_write_for(&self, within: Duration) -> Option<RwLockWriteGuard<'_, T>> {
        self.raw.write_within(within).then(|| self.writing())
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.raw.is_locked()
    }

    pub(crate) fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this guard is a read its thread took.
        unsafe { self.lock.raw.unlock_read() };
    }
}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: this guard is the write its thread took.
        unsafe { self.lock.raw.unlock_write() };
    }
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: a read is held, so no writer reaches the value.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: the write is held, so no other thread reaches the value.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` is the one reference through this guard.
        unsafe { &mut *self.lock.data.get() }
    }
}

// --- Cond ---------------------------------------------------------------------------------------

/// A condition on a [`Mutex`]: a sequence word that each notify moves.
pub(crate) struct Cond {
    seq: Word,
}

impl Cond {
    pub(crate) const fn new() -> Cond {
        Cond { seq: Word::new(0) }
    }

    /// Lets `guard`'s lock go, sleeps until a notify (or `within` passes, or for nothing), and
    /// takes the lock again: whether `within` passed.
    pub(crate) fn wait<T: ?Sized>(
        &self,
        guard: &mut MutexGuard<'_, T>,
        within: Option<Duration>,
    ) -> bool {
        // Read under the lock: a notify after a change the waiter has not seen moves it later.
        let seq = self.seq.value.load(Ordering::Relaxed);
        let raw = &guard.lock.raw;
        // SAFETY: the guard holds the lock; it is taken again below before the guard is used.
        unsafe { raw.unlock() };
        // wakes: `notify`, after the change the waiter waits for
        let timed_out = self.seq.wait(seq, within);
        raw.lock();
        timed_out
    }

    pub(crate) fn notify(&self, all: bool) {
        self.notify_with(all, Plant::None);
    }

    /// [`Cond::notify`], with a safeguard removed for a model test.
    pub(crate) fn notify_with(&self, all: bool, plant: Plant) {
        #[cfg(test)]
        let moves = plant != Plant::SequenceUnmoved;
        #[cfg(not(test))]
        let moves = {
            _ = plant;
            true
        };
        if moves {
            self.seq.value.fetch_add(1, Ordering::Relaxed);
        }
        self.seq.wake(all);
    }
}
