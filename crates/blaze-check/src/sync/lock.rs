//! `std::sync::{Mutex, RwLock, Condvar}`. In a model, taking a lock is a schedule point at
//! which a thread blocks while another holds it (`rt/locks.rs`); the data is in `std`'s lock,
//! which the model only ever hands to the thread it lets hold it, so `std`'s side never
//! contends. Releasing is no schedule point (as loom's): the releaser's next operation is.
#![expect(
    clippy::disallowed_types,
    reason = "the model of `std`'s locks, poisoning included, keeps the data in `std`'s own"
)]

pub mod lot;

use std::{
    fmt,
    ops::{Deref, DerefMut},
    panic::Location,
    sync::{LockResult, PoisonError, TryLockError, TryLockResult, atomic::AtomicU64 as Id},
    time::Duration,
};

use crate::rt::{self, At, Kind, Op, Wait};

/// Takes a model lock (`write`: exclusively): `false` outside a model.
fn acquire(id: &Id, at: At, what: &'static str, rw: Option<bool>) -> bool {
    loop {
        let took = rt::op(
            at,
            what,
            |rt| {
                let (obj, kind) = match rw {
                    None => {
                        let obj = rt.mutex(id);
                        rt.mutex_wait(obj)?;
                        (obj, Kind::Write)
                    }
                    Some(write) => {
                        let obj = rt.rwlock(id);
                        rt.rw_wait(obj, write)?;
                        (obj, if write { Kind::Write } else { Kind::Read })
                    }
                };
                Ok(Op { obj, kind })
            },
            |rt| {
                Ok(match rw {
                    None => {
                        let obj = rt.mutex(id);
                        rt.mutex_take(obj)
                    }
                    Some(write) => {
                        let obj = rt.rwlock(id);
                        rt.rw_take(obj, write)
                    }
                })
            },
        );
        match took {
            None => return false,
            Some(true) => return true,
            // Woken with the others when it was released, and another took it first.
            Some(false) => {}
        }
    }
}

/// Tries to take a model lock without blocking: `None` outside a model.
fn try_acquire(id: &Id, at: At, what: &'static str, rw: Option<bool>) -> Option<bool> {
    rt::op(
        at,
        what,
        |rt| {
            let obj = match rw {
                None => rt.mutex(id),
                Some(_) => rt.rwlock(id),
            };
            // Opaque: a waiter that blocks on the lock is a `Write` or `Read`; this one never
            // blocks.
            Ok(Op { obj, kind: Kind::Opaque })
        },
        |rt| {
            Ok(match rw {
                None => {
                    let obj = rt.mutex(id);
                    rt.mutex_take(obj)
                }
                Some(write) => {
                    let obj = rt.rwlock(id);
                    rt.rw_take(obj, write)
                }
            })
        },
    )
}

fn release_mutex(id: &Id) {
    rt::with(|rt| {
        let obj = rt.mutex(id);
        rt.mutex_release(obj);
    });
}

fn release_rw(id: &Id, write: bool) {
    rt::with(|rt| {
        let obj = rt.rwlock(id);
        rt.rw_release(obj, write);
    });
}

fn uncontended() -> ! {
    panic!("blaze-check: `std`'s lock under a model lock was held by another thread")
}

/// `std::sync::Mutex`.
pub struct Mutex<T: ?Sized> {
    id: Id,
    lock: std::sync::Mutex<T>,
}

/// `std::sync::MutexGuard`.
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct MutexGuard<'a, T: ?Sized + 'a> {
    mutex: &'a Mutex<T>,
    /// `std`'s guard; `None` only while it is dropped or handed to `Condvar::wait`.
    guard: Option<std::sync::MutexGuard<'a, T>>,
    model: bool,
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Mutex<T> {
        Mutex { id: Id::new(0), lock: std::sync::Mutex::new(value) }
    }

    pub fn into_inner(self) -> LockResult<T> {
        self.lock.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    fn guard<'a>(&'a self, guard: std::sync::MutexGuard<'a, T>, model: bool) -> MutexGuard<'a, T> {
        MutexGuard { mutex: self, guard: Some(guard), model }
    }

    fn wrap<'a>(
        &'a self,
        got: LockResult<std::sync::MutexGuard<'a, T>>,
        model: bool,
    ) -> LockResult<MutexGuard<'a, T>> {
        match got {
            Ok(guard) => Ok(self.guard(guard, model)),
            Err(poison) => Err(PoisonError::new(self.guard(poison.into_inner(), model))),
        }
    }

    /// `std`'s guard for a thread the model let take the lock.
    fn held(&self) -> LockResult<MutexGuard<'_, T>> {
        match self.lock.try_lock() {
            Ok(guard) => Ok(self.guard(guard, true)),
            Err(TryLockError::Poisoned(poison)) => {
                Err(PoisonError::new(self.guard(poison.into_inner(), true)))
            }
            Err(TryLockError::WouldBlock) => uncontended(),
        }
    }

    #[track_caller]
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        match acquire(&self.id, Location::caller(), "Mutex::lock", None) {
            true => self.held(),
            false => self.wrap(self.lock.lock(), false),
        }
    }

    #[track_caller]
    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        match try_acquire(&self.id, Location::caller(), "Mutex::try_lock", None) {
            Some(true) => self.held().map_err(TryLockError::Poisoned),
            Some(false) => Err(TryLockError::WouldBlock),
            None => match self.lock.try_lock() {
                Ok(guard) => Ok(self.guard(guard, false)),
                Err(TryLockError::Poisoned(poison)) => Err(TryLockError::Poisoned(
                    PoisonError::new(self.guard(poison.into_inner(), false)),
                )),
                Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
            },
        }
    }

    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        self.lock.get_mut()
    }

    pub fn is_poisoned(&self) -> bool {
        self.lock.is_poisoned()
    }

    pub fn clear_poison(&self) {
        self.lock.clear_poison();
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

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mutex").finish_non_exhaustive()
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.guard.as_deref().expect("a live guard holds the lock")
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_deref_mut().expect("a live guard holds the lock")
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // `std`'s lock first: the next holder the model picks takes it.
        if self.guard.take().is_some() && self.model {
            release_mutex(&self.mutex.id);
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// `std::sync::RwLock`.
pub struct RwLock<T: ?Sized> {
    id: Id,
    lock: std::sync::RwLock<T>,
}

/// `std::sync::RwLockReadGuard`.
#[must_use = "if unused the RwLock will immediately unlock"]
pub struct RwLockReadGuard<'a, T: ?Sized + 'a> {
    rwlock: &'a RwLock<T>,
    guard: Option<std::sync::RwLockReadGuard<'a, T>>,
    model: bool,
}

/// `std::sync::RwLockWriteGuard`.
#[must_use = "if unused the RwLock will immediately unlock"]
pub struct RwLockWriteGuard<'a, T: ?Sized + 'a> {
    rwlock: &'a RwLock<T>,
    guard: Option<std::sync::RwLockWriteGuard<'a, T>>,
    model: bool,
}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> RwLock<T> {
        RwLock { id: Id::new(0), lock: std::sync::RwLock::new(value) }
    }

    pub fn into_inner(self) -> LockResult<T> {
        self.lock.into_inner()
    }
}

impl<T: ?Sized> RwLock<T> {
    fn reader<'a>(&'a self, guard: std::sync::RwLockReadGuard<'a, T>, model: bool) -> RwLockReadGuard<'a, T> {
        RwLockReadGuard { rwlock: self, guard: Some(guard), model }
    }

    fn writer<'a>(&'a self, guard: std::sync::RwLockWriteGuard<'a, T>, model: bool) -> RwLockWriteGuard<'a, T> {
        RwLockWriteGuard { rwlock: self, guard: Some(guard), model }
    }

    fn held_read(&self) -> LockResult<RwLockReadGuard<'_, T>> {
        match self.lock.try_read() {
            Ok(guard) => Ok(self.reader(guard, true)),
            Err(TryLockError::Poisoned(poison)) => Err(PoisonError::new(self.reader(poison.into_inner(), true))),
            Err(TryLockError::WouldBlock) => uncontended(),
        }
    }

    fn held_write(&self) -> LockResult<RwLockWriteGuard<'_, T>> {
        match self.lock.try_write() {
            Ok(guard) => Ok(self.writer(guard, true)),
            Err(TryLockError::Poisoned(poison)) => Err(PoisonError::new(self.writer(poison.into_inner(), true))),
            Err(TryLockError::WouldBlock) => uncontended(),
        }
    }

    #[track_caller]
    pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> {
        if acquire(&self.id, Location::caller(), "RwLock::read", Some(false)) {
            return self.held_read();
        }
        match self.lock.read() {
            Ok(guard) => Ok(self.reader(guard, false)),
            Err(poison) => Err(PoisonError::new(self.reader(poison.into_inner(), false))),
        }
    }

    #[track_caller]
    pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, T>> {
        if acquire(&self.id, Location::caller(), "RwLock::write", Some(true)) {
            return self.held_write();
        }
        match self.lock.write() {
            Ok(guard) => Ok(self.writer(guard, false)),
            Err(poison) => Err(PoisonError::new(self.writer(poison.into_inner(), false))),
        }
    }

    #[track_caller]
    pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
        match try_acquire(&self.id, Location::caller(), "RwLock::try_read", Some(false)) {
            Some(true) => self.held_read().map_err(TryLockError::Poisoned),
            Some(false) => Err(TryLockError::WouldBlock),
            None => match self.lock.try_read() {
                Ok(guard) => Ok(self.reader(guard, false)),
                Err(TryLockError::Poisoned(poison)) => Err(TryLockError::Poisoned(
                    PoisonError::new(self.reader(poison.into_inner(), false)),
                )),
                Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
            },
        }
    }

    #[track_caller]
    pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
        match try_acquire(&self.id, Location::caller(), "RwLock::try_write", Some(true)) {
            Some(true) => self.held_write().map_err(TryLockError::Poisoned),
            Some(false) => Err(TryLockError::WouldBlock),
            None => match self.lock.try_write() {
                Ok(guard) => Ok(self.writer(guard, false)),
                Err(TryLockError::Poisoned(poison)) => Err(TryLockError::Poisoned(
                    PoisonError::new(self.writer(poison.into_inner(), false)),
                )),
                Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
            },
        }
    }

    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        self.lock.get_mut()
    }

    pub fn is_poisoned(&self) -> bool {
        self.lock.is_poisoned()
    }

    pub fn clear_poison(&self) {
        self.lock.clear_poison();
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

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLock").finish_non_exhaustive()
    }
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.guard.as_deref().expect("a live guard holds the lock")
    }
}

impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        if self.guard.take().is_some() && self.model {
            release_rw(&self.rwlock.id, false);
        }
    }
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.guard.as_deref().expect("a live guard holds the lock")
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_deref_mut().expect("a live guard holds the lock")
    }
}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        if self.guard.take().is_some() && self.model {
            release_rw(&self.rwlock.id, true);
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// `std::sync::WaitTimeoutResult` (whose constructor is `std`'s own).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WaitTimeoutResult(bool);

impl WaitTimeoutResult {
    pub fn timed_out(&self) -> bool {
        self.0
    }
}

/// `std::sync::Condvar`. In a model a wait releases the mutex and blocks until a notify, in one
/// step; a timed wait may also time out at once (a branch), which stands for every time it
/// could. Like loom, it explores no spurious wakeup of an untimed wait.
pub struct Condvar {
    id: Id,
    condvar: std::sync::Condvar,
}

impl Condvar {
    pub const fn new() -> Condvar {
        Condvar { id: Id::new(0), condvar: std::sync::Condvar::new() }
    }

    /// Releases the guard's model lock and waits: `true` if it timed out (only `timed`).
    fn model_wait<'a, T>(&self, guard: &mut MutexGuard<'a, T>, at: At, timed: bool) -> bool {
        // `std`'s lock goes first, as in `MutexGuard::drop`.
        drop(guard.guard.take());
        let lock: &'a Mutex<T> = guard.mutex;
        let mutex = &lock.id;
        // An access of the condvar, so DPOR orders it with each notify; releasing, enqueueing
        // and blocking then happen with no other thread in between.
        let waits = rt::op(
            at,
            "Condvar::wait",
            |rt| Ok(Op { obj: rt.condvar(&self.id), kind: Kind::Opaque }),
            |rt| {
                let condvar = rt.condvar(&self.id);
                let obj = rt.mutex(mutex);
                rt.mutex_release(obj);
                if timed && rt.path.branch_spurious().map_err(|stuck| rt.stuck(stuck))? {
                    return Ok(None);
                }
                rt.condvar_enqueue(condvar);
                Ok(Some(condvar))
            },
        )
        .expect("blaze-check: a model lock's guard used outside its model");
        let timed_out = match waits {
            Some(condvar) => {
                rt::block(Wait::Notify(condvar));
                false
            }
            None => true,
        };
        if !acquire(mutex, at, "Condvar::wait (lock again)", None) {
            panic!("blaze-check: a model lock's guard used outside its model");
        }
        guard.guard = match lock.lock.try_lock() {
            Ok(it) => Some(it),
            Err(TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
            Err(TryLockError::WouldBlock) => uncontended(),
        };
        timed_out
    }

    #[track_caller]
    pub fn wait<'a, T>(&self, mut guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
        if guard.model {
            self.model_wait(&mut guard, Location::caller(), false);
            return poisoned(guard);
        }
        let mutex = guard.mutex;
        let inner = guard.guard.take().expect("a live guard holds the lock");
        drop(guard);
        mutex.wrap(self.condvar.wait(inner), false)
    }

    #[track_caller]
    pub fn wait_while<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        mut condition: F,
    ) -> LockResult<MutexGuard<'a, T>>
    where
        F: FnMut(&mut T) -> bool,
    {
        while condition(&mut *guard) {
            guard = self.wait(guard)?;
        }
        Ok(guard)
    }

    #[track_caller]
    pub fn wait_timeout<'a, T>(
        &self,
        mut guard: MutexGuard<'a, T>,
        dur: Duration,
    ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)> {
        if guard.model {
            let timed_out = self.model_wait(&mut guard, Location::caller(), true);
            return match poisoned(guard) {
                Ok(guard) => Ok((guard, WaitTimeoutResult(timed_out))),
                Err(poison) => Err(PoisonError::new((poison.into_inner(), WaitTimeoutResult(timed_out)))),
            };
        }
        let mutex = guard.mutex;
        let inner = guard.guard.take().expect("a live guard holds the lock");
        drop(guard);
        match self.condvar.wait_timeout(inner, dur) {
            Ok((inner, out)) => Ok((mutex.guard(inner, false), WaitTimeoutResult(out.timed_out()))),
            Err(poison) => {
                let (inner, out) = poison.into_inner();
                Err(PoisonError::new((mutex.guard(inner, false), WaitTimeoutResult(out.timed_out()))))
            }
        }
    }

    #[track_caller]
    pub fn wait_timeout_while<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        dur: Duration,
        mut condition: F,
    ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)>
    where
        F: FnMut(&mut T) -> bool,
    {
        loop {
            if !condition(&mut *guard) {
                return Ok((guard, WaitTimeoutResult(false)));
            }
            let (next, out) = self.wait_timeout(guard, dur)?;
            guard = next;
            if out.timed_out() {
                let still = condition(&mut *guard);
                return Ok((guard, WaitTimeoutResult(still)));
            }
        }
    }

    #[track_caller]
    pub fn notify_one(&self) {
        self.notify(Location::caller(), false);
    }

    #[track_caller]
    pub fn notify_all(&self) {
        self.notify(Location::caller(), true);
    }

    fn notify(&self, at: At, all: bool) {
        let done = rt::op(
            at,
            if all { "Condvar::notify_all" } else { "Condvar::notify_one" },
            |rt| Ok(Op { obj: rt.condvar(&self.id), kind: Kind::Opaque }),
            |rt| {
                let obj = rt.condvar(&self.id);
                rt.condvar_notify(obj, all);
                Ok(())
            },
        );
        if done.is_none() {
            match all {
                true => self.condvar.notify_all(),
                false => self.condvar.notify_one(),
            }
        }
    }
}

/// A guard the model handed back, poisoned if `std`'s lock is.
fn poisoned<T: ?Sized>(guard: MutexGuard<'_, T>) -> LockResult<MutexGuard<'_, T>> {
    if guard.mutex.lock.is_poisoned() { Err(PoisonError::new(guard)) } else { Ok(guard) }
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
