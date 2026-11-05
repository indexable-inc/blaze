//! `parking_lot::{Mutex, RwLock, Condvar}`'s API over the model's locks, for a crate whose
//! `sync` switch is `parking_lot` outside a model (docs/v2/plan/lanes/safety/model.md, "The
//! switch"). A lock is taken as `parking_lot` takes it, with no poisoning (`std`'s lock under the
//! model's is taken as it is when a holder panicked), and a condition variable waits on the guard
//! in place. The model sees the same operations as through `std`'s API in [`super`].

use std::{
    fmt,
    panic::Location,
    sync::{PoisonError, TryLockError},
    time::{Duration, Instant},
};

pub use super::{MutexGuard, RwLockReadGuard, RwLockWriteGuard, WaitTimeoutResult};
use crate::rt::At;

/// `parking_lot::Mutex`.
pub struct Mutex<T: ?Sized>(super::Mutex<T>);

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Mutex<T> {
        Mutex(super::Mutex::new(value))
    }

    pub fn into_inner(self) -> T {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized> Mutex<T> {
    #[track_caller]
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        match self.0.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Mutex<T> {
        Mutex::new(T::default())
    }
}

impl<T> From<T> for Mutex<T> {
    fn from(value: T) -> Mutex<T> {
        Mutex::new(value)
    }
}

impl<T: ?Sized> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex").finish_non_exhaustive()
    }
}

/// `parking_lot::RwLock`.
pub struct RwLock<T: ?Sized>(super::RwLock<T>);

impl<T> RwLock<T> {
    pub const fn new(value: T) -> RwLock<T> {
        RwLock(super::RwLock::new(value))
    }

    pub fn into_inner(self) -> T {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: ?Sized> RwLock<T> {
    #[track_caller]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }

    #[track_caller]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.0.write().unwrap_or_else(PoisonError::into_inner)
    }

    #[track_caller]
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        match self.0.try_read() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    #[track_caller]
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        match self.0.try_write() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> RwLock<T> {
        RwLock::new(T::default())
    }
}

impl<T> From<T> for RwLock<T> {
    fn from(value: T) -> RwLock<T> {
        RwLock::new(value)
    }
}

impl<T: ?Sized> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLock").finish_non_exhaustive()
    }
}

/// `parking_lot::Condvar`: a wait releases and takes again the lock of the guard it is given,
/// which stays the caller's. `notify_one` and `notify_all` return nothing, as the model has no
/// count of the threads they woke.
pub struct Condvar(super::Condvar);

impl Condvar {
    pub const fn new() -> Condvar {
        Condvar(super::Condvar::new())
    }

    /// Waits on `guard`'s lock until a notify, or `dur` when given: whether it timed out.
    fn wait_in_place<T>(
        &self,
        guard: &mut MutexGuard<'_, T>,
        dur: Option<Duration>,
        at: At,
    ) -> bool {
        if guard.model {
            return self.0.model_wait(guard, at, dur.is_some());
        }
        let inner = guard.guard.take().expect("a live guard holds the lock");
        let (inner, timed_out) = match dur {
            None => (self.0.condvar.wait(inner).unwrap_or_else(PoisonError::into_inner), false),
            Some(dur) => {
                let (inner, out) =
                    self.0.condvar.wait_timeout(inner, dur).unwrap_or_else(PoisonError::into_inner);
                (inner, out.timed_out())
            }
        };
        guard.guard = Some(inner);
        timed_out
    }

    #[track_caller]
    pub fn wait<T>(&self, guard: &mut MutexGuard<'_, T>) {
        self.wait_in_place(guard, None, Location::caller());
    }

    #[track_caller]
    pub fn wait_for<T>(&self, guard: &mut MutexGuard<'_, T>, dur: Duration) -> WaitTimeoutResult {
        WaitTimeoutResult(self.wait_in_place(guard, Some(dur), Location::caller()))
    }

    #[track_caller]
    pub fn wait_until<T>(
        &self,
        guard: &mut MutexGuard<'_, T>,
        deadline: Instant,
    ) -> WaitTimeoutResult {
        let dur = deadline.saturating_duration_since(Instant::now());
        WaitTimeoutResult(self.wait_in_place(guard, Some(dur), Location::caller()))
    }

    #[track_caller]
    pub fn wait_while<T, F>(&self, guard: &mut MutexGuard<'_, T>, mut condition: F)
    where
        F: FnMut(&mut T) -> bool,
    {
        let at = Location::caller();
        while condition(&mut **guard) {
            self.wait_in_place(guard, None, at);
        }
    }

    /// As `parking_lot`'s: timed out only if `condition` still holds when `dur` has passed.
    #[track_caller]
    pub fn wait_while_for<T, F>(
        &self,
        guard: &mut MutexGuard<'_, T>,
        condition: F,
        dur: Duration,
    ) -> WaitTimeoutResult
    where
        F: FnMut(&mut T) -> bool,
    {
        self.wait_while_until_at(guard, condition, Instant::now() + dur, Location::caller())
    }

    #[track_caller]
    pub fn wait_while_until<T, F>(
        &self,
        guard: &mut MutexGuard<'_, T>,
        condition: F,
        deadline: Instant,
    ) -> WaitTimeoutResult
    where
        F: FnMut(&mut T) -> bool,
    {
        self.wait_while_until_at(guard, condition, deadline, Location::caller())
    }

    fn wait_while_until_at<T, F>(
        &self,
        guard: &mut MutexGuard<'_, T>,
        mut condition: F,
        deadline: Instant,
        at: At,
    ) -> WaitTimeoutResult
    where
        F: FnMut(&mut T) -> bool,
    {
        while condition(&mut **guard) {
            let dur = deadline.saturating_duration_since(Instant::now());
            if self.wait_in_place(guard, Some(dur), at) {
                return WaitTimeoutResult(condition(&mut **guard));
            }
        }
        WaitTimeoutResult(false)
    }

    #[track_caller]
    pub fn notify_one(&self) {
        self.0.notify(Location::caller(), false);
    }

    #[track_caller]
    pub fn notify_all(&self) {
        self.0.notify(Location::caller(), true);
    }
}

impl Default for Condvar {
    fn default() -> Condvar {
        Condvar::new()
    }
}

impl fmt::Debug for Condvar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Condvar").finish_non_exhaustive()
    }
}
