//! The in-flight table [`super::Flights`] replaced: runs by key in a hash map per shard behind a
//! lock. Kept for tests only, as the oracle the table is checked against and the bench's
//! baseline.

use std::{
    cell::UnsafeCell,
    collections::hash_map::Entry,
    ptr,
    sync::atomic::{AtomicU8, Ordering},
};

use super::{Lock, claim_flight, flight, give_way, hex, task};
use crate::{
    Cancelled, Key,
    sched::{self, CounterId, Wait},
};

type ByKey<V> = blaze_hash::ByHash<Key, V>;

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const GAVE_WAY: u8 = 2;

struct Waiter<V> {
    next: UnsafeCell<*const Waiter<V>>,
    value: UnsafeCell<Option<V>>,
    state: AtomicU8,
}

struct Waiters<V>(*const Waiter<V>);

// SAFETY: as the replaced design's: reached only under the shard's lock or by the runner.
unsafe impl<V: Send> Send for Waiters<V> {}

struct InFlight<V> {
    runner: u64,
    flight: u64,
    scope: Option<CounterId>,
    waiters: Waiters<V>,
}

#[repr(align(128))]
struct Shard<V>(Lock<ByKey<InFlight<V>>>);

pub(super) struct Sharded<V> {
    shards: Box<[Shard<V>]>,
    mask: usize,
}

impl<V: Clone + Send + Sync + 'static> Sharded<V> {
    pub(super) fn new() -> Sharded<V> {
        let count = (4 * std::thread::available_parallelism().map_or(1, std::num::NonZero::get))
            .next_power_of_two()
            .max(64);
        Sharded { shards: (0..count).map(|_| Shard(Lock::new(ByKey::default()))).collect(), mask: count - 1 }
    }

    fn shard(&self, key: &Key) -> &Lock<ByKey<InFlight<V>>> {
        let mut word = [0; 8];
        word.copy_from_slice(&key.0[8..16]);
        &self.shards[u64::from_le_bytes(word) as usize & self.mask].0
    }

    fn entry(me: u64) -> InFlight<V> {
        InFlight { runner: me, flight: 0, scope: sched::innermost_scope(), waiters: Waiters(ptr::null()) }
    }

    pub(super) fn get_or_run(&self, key: Key, run: impl FnOnce() -> Result<V, Cancelled>) -> Result<V, Cancelled> {
        sched::miss();
        let me = task(sched::job_here());
        loop {
            let waiter = Waiter { next: UnsafeCell::new(ptr::null()), value: UnsafeCell::new(None), state: AtomicU8::new(RUNNING) };
            let number = {
                let mut shard = self.shard(&key).lock();
                let entry = match shard.entry(key) {
                    Entry::Vacant(slot) => {
                        slot.insert(Self::entry(me));
                        break;
                    }
                    Entry::Occupied(slot) => slot.into_mut(),
                };
                assert!(entry.runner != me, "key {} is asked by the task that runs it: its value depends on itself", hex(&key));
                if entry.flight == 0 {
                    entry.flight = claim_flight(entry.runner, entry.scope);
                }
                // SAFETY: the waiter is linked under the lock and stays until its end is written
                // or it unlinks itself below.
                unsafe { *waiter.next.get() = entry.waiters.0 };
                entry.waiters.0 = ptr::from_ref(&waiter);
                entry.flight
            };
            let refused = waiter.state.load(Ordering::Acquire) == RUNNING && sched::wait(Wait::Key(flight(number))).is_err();
            if refused {
                self.unlink(&key, &waiter);
                return Err(give_way());
            }
            match waiter.state.load(Ordering::Acquire) {
                // SAFETY: written before `DONE`, which this load read.
                DONE => return Ok(unsafe { (*waiter.value.get()).take() }.expect("a value with DONE")),
                _ => continue,
            }
        }
        let result = run();
        self.end(&key, result.as_ref().ok());
        result
    }

    fn unlink(&self, key: &Key, waiter: &Waiter<V>) {
        {
            let mut shard = self.shard(key).lock();
            if let Some(entry) = shard.get_mut(key) {
                let mut link: *mut *const Waiter<V> = &raw mut entry.waiters.0;
                loop {
                    // SAFETY: as the replaced design's: every linked waiter is in place under the lock.
                    let at = unsafe { *link };
                    if at.is_null() {
                        break;
                    }
                    if at == ptr::from_ref(waiter) {
                        // SAFETY: as above; this waiter is linked and in place under the lock.
                        let next = unsafe { *waiter.next.get() };
                        // SAFETY: as above; `link` names a link held in place under the lock.
                        unsafe { *link = next };
                        return;
                    }
                    // SAFETY: as above.
                    link = unsafe { &*at }.next.get();
                }
            }
        }
        while waiter.state.load(Ordering::Acquire) == RUNNING {
            std::thread::yield_now();
        }
    }

    pub(super) fn run_if_idle(&self, key: Key, run: impl FnOnce() -> Result<V, Cancelled>) -> Option<Result<V, Cancelled>> {
        self.hold(key)?;
        let result = run();
        self.end(&key, result.as_ref().ok());
        Some(result)
    }

    /// Claims `key` for this task; `None` when another task runs it.
    pub(super) fn hold(&self, key: Key) -> Option<()> {
        let me = task(sched::job_here());
        match self.shard(&key).lock().entry(key) {
            Entry::Vacant(slot) => _ = slot.insert(Self::entry(me)),
            Entry::Occupied(slot) => {
                assert!(slot.get().runner != me, "key {} is held by the task that holds it already", hex(&key));
                return None;
            }
        }
        Some(())
    }

    /// Ends this task's run of `key` on `value`, or a cancel.
    pub(super) fn end(&self, key: &Key, value: Option<&V>) {
        let Some(InFlight { flight: number, waiters, .. }) = self.shard(key).lock().remove(key) else { return };
        let state = if value.is_some() { DONE } else { GAVE_WAY };
        let mut at = waiters.0;
        // SAFETY: as the replaced design's: the list is the runner's once taken out.
        while let Some(waiter) = unsafe { at.as_ref() } {
            // SAFETY: as above.
            let next = unsafe { *waiter.next.get() };
            if let Some(value) = value {
                // SAFETY: as above.
                unsafe { *waiter.value.get() = Some(value.clone()) };
            }
            waiter.state.store(state, Ordering::Release);
            at = next;
        }
        if number != 0 {
            sched::key_done(flight(number));
        }
    }

    pub(super) fn running(&self) -> usize {
        self.shards.iter().map(|it| it.0.lock().len()).sum()
    }

    pub(super) fn waiters(&self, key: &Key) -> usize {
        let shard = self.shard(key).lock();
        let mut at = shard.get(key).map_or(ptr::null(), |it| it.waiters.0);
        let mut count = 0;
        // SAFETY: under the lock each linked waiter is in place.
        while let Some(waiter) = unsafe { at.as_ref() } {
            count += 1;
            // SAFETY: as above.
            at = unsafe { *waiter.next.get() };
        }
        count
    }
}
