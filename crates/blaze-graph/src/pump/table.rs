//! The runs under way of [`super::Flights`], by key: a table of buckets, a key's run in an
//! entry of its home bucket's chain while it runs and gone once it ends.
//!
//! - A bucket is one 128-byte line pair: a latch, two entries' run words, and the entries'
//!   keys and waiters. A chain is a home bucket and the buckets added behind it once both
//!   entries were held; only the home bucket's latch is taken, and it guards the whole chain.
//! - The run word is the runner's task, its innermost scope as the run began, and whether
//!   anyone waits; 0 is a free entry. A new run takes the latch, finds no entry of its key, and
//!   writes the key into a free one: two askers of a key can't both miss each other. A run
//!   nobody waits for ends with one CAS of its run word to 0, taking no latch, and the entry is
//!   free again: a table of N runs under way holds N entries, whatever keys came and went.
//! - A waiter takes the latch, marks the run waited, pushes itself (a node on its own stack)
//!   and claims the run's flight in the scheduler if no waiter did yet. The runner's CAS then
//!   fails, and it takes the latch to take the list out and free the entry. A waiter that gives
//!   way unlinks itself under the latch.
//! - Once the buckets added behind home buckets pass an eighth of the table, it grows to twice
//!   as many: each chain, under its latch, moves to the next table (each run word swapped for
//!   `MOVED`, which fails its runner's CAS, sending its end to the latch) and is marked moved;
//!   then the next table is published. An operation that finds its chain moved waits for it.
//!   The table replaced is kept until the runs are dropped, so nothing that read it, nor a
//!   claim's run word, is ever left dangling, at no more than the size of the table now.

use std::{cell::UnsafeCell, iter, marker::PhantomData, mem, ptr};

use crossbeam_queue::SegQueue;

use super::Lock;
use crate::{
    Key,
    sched::CounterId,
    spin::SpinWait,
    sync::{
        MutexGuard,
        atomic::{AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
};

pub(super) const RUNNING: u8 = 0;
pub(super) const DONE: u8 = 1;
pub(super) const GAVE_WAY: u8 = 2;

/// A wait for a run, on the waiter's own stack while it waits, linked into its entry's list.
/// The runner reads it once it has taken the list out, writes it its value and its state, and
/// touches it no more: `state` is the last write, and the waiter leaves only once it reads
/// that, or once it has unlinked itself ([`Runs::leave`]).
pub(super) struct Waiter<V> {
    /// The waiter linked before this one: written under the chain's latch, read under it or by
    /// the runner once the list is its own.
    next: UnsafeCell<*const Waiter<V>>,
    /// The run's value, written by the runner before `state` says `DONE`.
    value: UnsafeCell<Option<V>>,
    state: AtomicU8,
}

impl<V> Waiter<V> {
    pub(super) fn new() -> Waiter<V> {
        Waiter { next: UnsafeCell::new(ptr::null()), value: UnsafeCell::new(None), state: AtomicU8::new(RUNNING) }
    }

    pub(super) fn is_running(&self) -> bool {
        self.state.load(Ordering::Acquire) == RUNNING
    }

    /// The run's end once its runner wrote it: its value, `None` for a cancel.
    pub(super) fn ended(&self) -> Option<Option<V>> {
        match self.state.load(Ordering::Acquire) {
            RUNNING => None,
            // SAFETY: the runner wrote the value before it stored `DONE`, which this load read,
            // and touches the waiter no more; only the waiter reads it.
            DONE => Some(unsafe { (*self.value.get()).take() }),
            _ => Some(None),
        }
    }
}

/// The run word's runner task ([`super::task`]): 0 for no run. A task is below it, so no run
/// word is [`MOVED`].
const TASK: u64 = (1 << 31) - 1;
/// Set by the first waiter, so the runner's CAS to 0 fails and it takes the list.
const WAITED: u64 = 1 << 31;
/// The scope half of a run word begun inside no scope.
const NO_SCOPE: u32 = u32::MAX;
/// The run word of an entry a grow moved to the next table.
const MOVED: u64 = u64::MAX;
/// A bucket's entries: with its latch and run words, a 128-byte line pair.
const ENTRIES: usize = 2;

fn word(key: &Key, at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&key.0[at..at + 8]);
    u64::from_le_bytes(word)
}

/// A key's home bucket, from all four of its words, mixed by murmur3's finalizer: a key is a
/// content hash, but keys a caller numbers (a test's, a bench's) differ in a few bits of one
/// word, and a linear mix walks consecutive ones through the buckets in step, so two threads
/// a few keys apart keep meeting in one bucket.
fn home(key: &Key, mask: usize) -> usize {
    let mut mixed = word(key, 0) ^ word(key, 8).rotate_left(16) ^ word(key, 16).rotate_left(32) ^ word(key, 24).rotate_left(48);
    mixed ^= mixed >> 33;
    mixed = mixed.wrapping_mul(0xff51_afd7_ed55_8ccd);
    mixed ^= mixed >> 33;
    mixed = mixed.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    mixed ^= mixed >> 33;
    mixed as usize & mask
}

/// The run word of `task`'s run of a key, begun inside `scope`.
fn run_word(task: u64, scope: Option<CounterId>) -> u64 {
    assert!(task != 0 && task < TASK, "task {task:#x} does not fit a run word");
    let scope = scope.map_or(NO_SCOPE, |it| {
        assert_ne!(it.0, NO_SCOPE, "a scope's counter is not the run word's none");
        it.0
    });
    u64::from(scope) << 32 | task
}

fn scope_of(run: u64) -> Option<CounterId> {
    let scope = (run >> 32) as u32;
    (scope != NO_SCOPE).then_some(CounterId(scope))
}

/// An entry's waiters, the last linked first (null for none), and its run's flight in the
/// scheduler (0 until a waiter claims one). A free entry has neither.
struct Waiting<V> {
    head: *const Waiter<V>,
    flight: u64,
}

impl<V> Waiting<V> {
    const NONE: Waiting<V> = Waiting { head: ptr::null(), flight: 0 };
}

/// What a chain's latch guards, of each bucket of the chain.
struct Entries<V> {
    keys: [Key; ENTRIES],
    waiting: [Waiting<V>; ENTRIES],
    /// The bucket added behind this one, once both its entries were held.
    next: Option<Box<Bucket<V>>>,
}

#[repr(align(128))]
struct Bucket<V> {
    /// Taken on a home bucket only, for its chain; says whether a grow moved the chain.
    latch: Lock<bool>,
    /// Each entry's run word: written under the latch, but for the end of a run nobody waits
    /// for, one CAS to 0 by its runner.
    runs: [AtomicU64; ENTRIES],
    entries: UnsafeCell<Entries<V>>,
}

// SAFETY: a bucket's entries are reached only under its chain's latch, and its waiters only
// there or by the runner that took them out, which sends each a `V` (hence `V: Send`).
unsafe impl<V: Send> Send for Bucket<V> {}
// SAFETY: as above.
unsafe impl<V: Send> Sync for Bucket<V> {}

impl<V> Bucket<V> {
    fn new() -> Bucket<V> {
        Bucket {
            latch: Lock::new(false),
            runs: [AtomicU64::new(0), AtomicU64::new(0)],
            entries: UnsafeCell::new(Entries {
                keys: [Key([0; 32]); ENTRIES],
                waiting: [Waiting::NONE, Waiting::NONE],
                next: None,
            }),
        }
    }
}

/// The buckets of `home`'s chain, `home` first.
///
/// # Safety
///
/// The chain's latch is held, or no other thread has its table, while the iterator is used.
unsafe fn chain<V>(home: &Bucket<V>) -> impl Iterator<Item = &Bucket<V>> {
    // SAFETY: the caller's; a bucket added to a chain stays in it while its table lives.
    iter::successors(Some(home), |it| unsafe { (*it.entries.get()).next.as_deref() })
}

struct Table<V> {
    buckets: Box<[Bucket<V>]>,
    /// Buckets added behind home buckets: never fewer while the table is current.
    added: AtomicUsize,
}

impl<V> Table<V> {
    fn new(len: usize) -> Table<V> {
        // alloc: a table, at a grow
        Table { buckets: (0..len).map(|_| Bucket::new()).collect(), added: AtomicUsize::new(0) }
    }

    fn home(&self, key: &Key) -> &Bucket<V> {
        &self.buckets[home(key, self.buckets.len() - 1)]
    }

    /// Past an eighth of its buckets added, a table grows.
    fn crowded(&self) -> bool {
        self.added.load(Ordering::Relaxed) > self.buckets.len() / 8
    }

    /// A free entry of `home`'s chain, in a bucket added at its end when every one is held;
    /// and whether one was added.
    ///
    /// # Safety
    ///
    /// As for [`chain`].
    unsafe fn free<'a>(&self, home: &'a Bucket<V>) -> (&'a Bucket<V>, usize, bool) {
        let mut last = home;
        // SAFETY: the caller's.
        for bucket in unsafe { chain(home) } {
            if let Some(at) = (0..ENTRIES).find(|&at| bucket.runs[at].load(Ordering::Acquire) == 0) {
                return (bucket, at, false);
            }
            last = bucket;
        }
        // SAFETY: the caller's; no reference into `last`'s entries is held.
        let entries = unsafe { &mut *last.entries.get() };
        // alloc: a bucket added to a chain, kept while the table lives
        let added = entries.next.insert(Box::new(Bucket::new()));
        self.added.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the bucket is in the chain from now on, so it lives as long as `home`.
        (unsafe { &*ptr::from_ref::<Bucket<V>>(added) }, 0, true)
    }

    /// Puts a run a grow moves into this table, which no other thread has yet.
    fn put(&self, key: Key, run: u64, waiting: Waiting<V>) {
        // SAFETY: no other thread has the table.
        let (bucket, at, _) = unsafe { self.free(self.home(&key)) };
        // SAFETY: as above.
        let entries = unsafe { &mut *bucket.entries.get() };
        entries.keys[at] = key;
        entries.waiting[at] = waiting;
        bucket.runs[at].store(run, Ordering::Relaxed);
    }
}

/// A key's chain, its latch held, in a table that has not moved it.
struct Latched<'a, V> {
    table: &'a Table<V>,
    home: &'a Bucket<V>,
    _held: MutexGuard<'a, bool>,
}

impl<'a, V> Latched<'a, V> {
    fn chain(&self) -> impl Iterator<Item = &'a Bucket<V>> {
        // SAFETY: the latch is held while `self` lives, and the iterator borrows it.
        unsafe { chain(self.home) }
    }

    /// The entry of `key`'s run, while it runs.
    fn find(&self, key: &Key) -> Option<(&'a Bucket<V>, usize)> {
        self.chain().find_map(|bucket| {
            (0..ENTRIES)
                .find(|&at| {
                    // SAFETY: the keys are written only under the latch, held.
                    bucket.runs[at].load(Ordering::Acquire) != 0 && unsafe { (*bucket.entries.get()).keys[at] } == *key
                })
                .map(|at| (bucket, at))
        })
    }

    /// The run of `key` by `word`, in a free entry; and whether the chain grew for it.
    fn insert(&self, key: Key, word: u64) -> (Claim<'a>, bool) {
        // SAFETY: the latch is held.
        let (bucket, at, added) = unsafe { self.table.free(self.home) };
        // SAFETY: as above; a free entry's waiting is none already.
        unsafe { (*bucket.entries.get()).keys[at] = key };
        bucket.runs[at].store(word, Ordering::Release);
        (Claim { key, word, run: &bucket.runs[at] }, added)
    }

    /// The waiters and flight of the entry `at` of `bucket`, a bucket of this chain.
    #[expect(clippy::mut_from_ref, reason = "the latch held makes the entry this thread's alone")]
    fn waiting(&self, bucket: &'a Bucket<V>, at: usize) -> &mut Waiting<V> {
        // SAFETY: the latch is held, and the entry is borrowed no other way meanwhile.
        unsafe { &mut (*bucket.entries.get()).waiting[at] }
    }
}

/// The runs under way, by key.
pub(super) struct Runs<V> {
    /// Owned (`Box::into_raw`); replaced only by [`Runs::grow`].
    table: AtomicPtr<Table<V>>,
    /// The tables grows replaced, freed with the runs: an operation that read one before its
    /// grow, or a claim of a run in it, may be using it still.
    retired: SegQueue<Box<Table<V>>>,
    /// Held by a grow, and by a count of the runs, which sees each run once.
    grow: Lock<()>,
    _owns: PhantomData<Box<Table<V>>>,
}

/// A run claimed: its key, its run word, and its entry's run word.
pub(super) struct Claim<'a> {
    key: Key,
    word: u64,
    run: &'a AtomicU64,
}

pub(super) enum Asked<'a> {
    /// The asker runs the key.
    Run(Claim<'a>),
    /// The asker is linked as a waiter of the run by task `runner`, whose flight in the
    /// scheduler is `flight`.
    Wait { flight: u64, runner: u64 },
    /// The asker runs the key already.
    Itself,
}

pub(super) enum Idle<'a> {
    Claimed(Claim<'a>),
    /// Another task runs the key.
    Busy,
    Itself,
}

impl<V> Runs<V> {
    /// An empty table of `buckets` buckets, a power of two.
    pub(super) fn new(buckets: usize) -> Runs<V> {
        assert!(buckets.is_power_of_two(), "a table's buckets are a power of two");
        Runs {
            // alloc: the first table, once
            table: AtomicPtr::new(Box::into_raw(Box::new(Table::new(buckets)))),
            retired: SegQueue::new(),
            grow: Lock::new(()),
            _owns: PhantomData,
        }
    }

    fn current(&self) -> &Table<V> {
        // SAFETY: a table is freed only with the runs (`retired`, `Drop`), so it outlives
        // `&self`.
        unsafe { &*self.table.load(Ordering::Acquire) }
    }

    /// `key`'s chain, latched, in the current table.
    fn latch(&self, key: &Key) -> Latched<'_, V> {
        loop {
            let table = self.current();
            let home = table.home(key);
            let held = home.latch.lock_said("a key's bucket of the table of runs");
            if !*held {
                return Latched { table, home, _held: held };
            }
            drop(held);
            // A grow moved the chain, and publishes the next table once it moved them all.
            SpinWait::until("a grow of the table of runs to publish the next table", || {
                !ptr::eq(self.table.load(Ordering::Acquire), table)
            });
        }
    }

    /// The run of `key` by `word`, in a free entry of its latched chain; grows the table after
    /// when the chain grew past what the table holds.
    fn insert<'a>(&'a self, latched: Latched<'a, V>, key: Key, word: u64) -> Claim<'a> {
        let (claim, added) = latched.insert(key, word);
        let table = latched.table;
        drop(latched);
        if added && table.crowded() {
            self.grow(table);
        }
        claim
    }

    /// Asks for `key` as task `me`, inside `scope`: claims its run, or links `waiter` to the run
    /// under way, `flight` claiming that run's flight in the scheduler (for its runner and the
    /// scope its run began inside) if no waiter did yet.
    pub(super) fn ask(
        &self,
        key: Key,
        me: u64,
        scope: Option<CounterId>,
        waiter: &Waiter<V>,
        flight: &mut impl FnMut(u64, Option<CounterId>) -> u64,
    ) -> Asked<'_> {
        let mine = run_word(me, scope);
        let mut paused = false;
        let (latched, bucket, at, seen) = 'ask: loop {
            let latched = self.latch(&key);
            let Some((bucket, at)) = latched.find(&key) else { return Asked::Run(self.insert(latched, key, mine)) };
            let run = &bucket.runs[at];
            let mut seen = run.load(Ordering::Acquire);
            loop {
                if seen == 0 {
                    // Its run ended since, leaving the key in the free entry; only a holder of
                    // the latch writes a free entry's run word.
                    run.store(mine, Ordering::Release);
                    return Asked::Run(Claim { key, word: mine, run });
                }
                if seen & TASK == me {
                    return Asked::Itself;
                }
                if seen & WAITED != 0 {
                    break 'ask (latched, bucket, at, seen);
                }
                if !paused {
                    // A run of a few stores ends sooner than its flight is claimed and its
                    // waiter suspended and woken: spin a little on its word, outside the latch,
                    // and ask again.
                    paused = true;
                    drop(latched);
                    let mut spin = SpinWait::new();
                    while run.load(Ordering::Acquire) == seen && spin.pause() {}
                    continue 'ask;
                }
                match run.compare_exchange(seen, seen | WAITED, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break 'ask (latched, bucket, at, seen),
                    Err(now) => seen = now,
                }
            }
        };
        let waiting = latched.waiting(bucket, at);
        // Before the waiter is linked: a panic of the claim leaves nothing linked.
        if waiting.flight == 0 {
            waiting.flight = flight(seen & TASK, scope_of(seen));
        }
        // SAFETY: the waiter's `next` is its own until it is linked, here, under the latch;
        // its caller keeps it in place until the runner wrote its end or it left
        // ([`Runs::leave`]).
        unsafe { *waiter.next.get() = waiting.head };
        waiting.head = ptr::from_ref(waiter);
        Asked::Wait { flight: waiting.flight, runner: seen & TASK }
    }

    /// Claims `key`'s run for task `me`, inside `scope`, if no task runs it now.
    pub(super) fn claim_if_idle(&self, key: Key, me: u64, scope: Option<CounterId>) -> Idle<'_> {
        let mine = run_word(me, scope);
        let latched = self.latch(&key);
        let Some((bucket, at)) = latched.find(&key) else { return Idle::Claimed(self.insert(latched, key, mine)) };
        let run = &bucket.runs[at];
        match run.load(Ordering::Acquire) {
            0 => {
                // As in `ask`: ended since, and the latch held.
                run.store(mine, Ordering::Release);
                Idle::Claimed(Claim { key, word: mine, run })
            }
            seen if seen & TASK == me => Idle::Itself,
            _ => Idle::Busy,
        }
    }

    /// Ends `claim`'s run: each waiter is written `value`'s clone, or a cancel for `None`. The
    /// run's flight, if a waiter claimed one, for the caller to end now that its waiters are
    /// written.
    pub(super) fn end(&self, claim: &Claim<'_>, value: Option<&V>) -> Option<u64>
    where
        V: Clone,
    {
        if claim.run.compare_exchange(claim.word, 0, Ordering::Release, Ordering::Relaxed).is_ok() {
            return None;
        }
        // Waited for, or moved by a grow: the run's entry, in the table now.
        let waiting = {
            let latched = self.latch(&claim.key);
            let (bucket, at) = latched.find(&claim.key).expect("a run's entry stays until it ends");
            debug_assert_eq!(
                bucket.runs[at].load(Ordering::Relaxed) & !WAITED,
                claim.word,
                "a run's word is its own until it ends"
            );
            let waiting = mem::replace(latched.waiting(bucket, at), Waiting::NONE);
            bucket.runs[at].store(0, Ordering::Release);
            waiting
        };
        let state = if value.is_some() { DONE } else { GAVE_WAY };
        let mut at = waiting.head;
        // SAFETY: the list was taken out under the latch, so no waiter of it is unlinked or
        // leaves before this writes its state, and no other thread writes a `next` of it. The
        // waiter is in place until its state is stored, and everything read of it is read
        // before that.
        while let Some(waiter) = unsafe { at.as_ref() } {
            // SAFETY: as above.
            let next = unsafe { *waiter.next.get() };
            if let Some(value) = value {
                // clone: each waiter's own value
                let value = Some(value.clone());
                // SAFETY: the waiter reads `value` only once it loaded `DONE`, stored below, and
                // nobody else writes it.
                unsafe { *waiter.value.get() = value };
            }
            waiter.state.store(state, Ordering::Release);
            at = next;
        }
        (waiting.flight != 0).then_some(waiting.flight)
    }

    /// Takes `waiter`, which gives up its wait for `key` (it gave way, or unwinds), out of its
    /// run's list; or, once the runner took the list, waits for the runner to write it its end.
    /// Either way the runner touches it no more once this returns.
    pub(super) fn leave(&self, key: &Key, waiter: &Waiter<V>) {
        if !waiter.is_running() {
            return;
        }
        {
            let latched = self.latch(key);
            if let Some((bucket, at)) = latched.find(key) {
                let waiting = latched.waiting(bucket, at);
                let me = ptr::from_ref(waiter);
                let mut link: *mut *const Waiter<V> = &raw mut waiting.head;
                loop {
                    // SAFETY: `link` is the list's head or the `next` of a waiter of it; under
                    // the latch, held here, every waiter of the list is linked and in place, and
                    // its `next` is read and written only under the latch.
                    let here = unsafe { *link };
                    if here.is_null() {
                        break;
                    }
                    if here == me {
                        // SAFETY: as above; this waiter's `next` is read only under the latch.
                        let next = unsafe { *waiter.next.get() };
                        // SAFETY: as above; `link` names a list link held in place under the latch.
                        unsafe { *link = next };
                        return;
                    }
                    // SAFETY: as above.
                    link = unsafe { &*here }.next.get();
                }
            }
        }
        // The run ended and took the list: its runner is writing each waiter its end.
        SpinWait::until("a run's end to write its waiter, whose list it took", || !waiter.is_running());
    }

    /// How many keys are running now.
    pub(super) fn running(&self) -> usize {
        let _grow = self.grow.lock();
        let table = self.current();
        table
            .buckets
            .iter()
            .map(|home| {
                let latched = Latched { table, home, _held: home.latch.lock() };
                latched.chain().map(|it| it.runs.iter().filter(|it| it.load(Ordering::Acquire) != 0).count()).sum::<usize>()
            })
            .sum()
    }

    /// How many waiters `key`'s run has linked now.
    pub(super) fn waiters(&self, key: &Key) -> usize {
        let latched = self.latch(key);
        let Some((bucket, at)) = latched.find(key) else { return 0 };
        let (mut count, mut at) = (0, latched.waiting(bucket, at).head);
        // SAFETY: under the latch each linked waiter is in place.
        while let Some(waiter) = unsafe { at.as_ref() } {
            count += 1;
            // SAFETY: as above.
            at = unsafe { *waiter.next.get() };
        }
        count
    }

    /// The table's buckets and the buckets added behind them, for tests of its sizing.
    #[cfg(test)]
    pub(super) fn size(&self) -> (usize, usize) {
        let table = self.current();
        (table.buckets.len(), table.added.load(Ordering::Relaxed))
    }

    /// Moves every run under way to a table of twice `from`'s buckets, unless another grow
    /// replaced `from` already.
    fn grow(&self, from: &Table<V>) {
        let _grow = self.grow.lock();
        if !ptr::eq(self.table.load(Ordering::Acquire), from) {
            return;
        }
        // alloc: the next table, at a grow
        let next = Box::new(Table::new(2 * from.buckets.len()));
        for home in &from.buckets {
            let mut moved = home.latch.lock_said("a key's bucket of the table of runs, to move it");
            // SAFETY: the latch is held.
            for bucket in unsafe { chain(home) } {
                for at in 0..ENTRIES {
                    let run = &bucket.runs[at];
                    let mut seen = run.load(Ordering::Acquire);
                    // Only the run's end changes a held run word without the latch: to 0.
                    while seen != 0 {
                        match run.compare_exchange(seen, MOVED, Ordering::AcqRel, Ordering::Acquire) {
                            Ok(_) => break,
                            Err(now) => seen = now,
                        }
                    }
                    if seen != 0 {
                        // SAFETY: the latch is held, and nothing else of the entry is borrowed.
                        let entries = unsafe { &mut *bucket.entries.get() };
                        next.put(entries.keys[at], seen, mem::replace(&mut entries.waiting[at], Waiting::NONE));
                    }
                }
            }
            *moved = true;
        }
        let old = self.table.swap(Box::into_raw(next), Ordering::AcqRel);
        // SAFETY: the table came from `Box::into_raw`, and only this grow, under its lock,
        // replaced it.
        self.retired.push(unsafe { Box::from_raw(old) });
    }
}

impl<V> Drop for Runs<V> {
    fn drop(&mut self) {
        // SAFETY: the table came from `Box::into_raw`, and `&mut self` means no operation is
        // under way.
        drop(unsafe { Box::from_raw(self.table.load(Ordering::Acquire)) });
    }
}
