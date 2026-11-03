//! The trait solver's caches of solved goals, in the two scopes of
//! [`CacheScope`](rustc_type_ir::search_graph::CacheScope):
//!
//! - [`shared`]: a goal whose reads `blaze_query::db::capture_reads` recorded, for every query and
//!   thread of one database, crate and revision. A query that takes an answer from it replays
//!   the reads (`Tracked`), so it reruns when they change, as rustc's `with_cached_task` /
//!   `get_tracked` do.
//! - [`of_query`]: a goal no other query may take on (its capture is `None`), one cache for
//!   each interner, in a table every thread shares, so a query that moves to another thread
//!   keeps what it solved.
//!
//! Both are the one [`Shared`] table: a lookup is a few loads under the task's
//! `blaze_reclaim` guard, with no lock and no write but a use mark set once per sweep. A
//! shared cache is replaced by the first use in a later revision (a query's never sees one),
//! and a cache given away is freed once no task can still hold it.


use super::DbInterner;
use base_db::{Crate, Db, Nonce};
use blaze_hash::FixedState;
use blaze_mem::{Cache, Pool, Pooled, Tag, pool::Limbo};
use blaze_query::db::View;
use blaze_reclaim::Horizon;
use parking_lot::Mutex;
use rustc_type_ir::search_graph::GlobalCache;
use std::{
    cell::Cell,
    hash::BuildHasher,
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
};

/// The search graph's cache, with the lifetime of the interner it is used through erased: it
/// holds interned values (types, predicates, canonical inputs and responses), handles into
/// process-wide arenas that are never freed and that every thread reads, so it is the same
/// for any `'db`.
type Erased = GlobalCache<DbInterner<'static>>;

// Shared by every thread with no lock: by the auto traits, which this checks.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Erased>();
    send_sync::<SharedCaches>();
};

/// `cache`, as the cache of an interner of `'db`.
#[inline]
fn unerase<'a, 'db>(cache: &'a Erased) -> &'a GlobalCache<DbInterner<'db>> {
    // SAFETY: `'db` only borrows the database; the interned values the cache holds do not
    // depend on it (see `Erased`). A cache is handed only to interners of the database and
    // revision it was filled in (`shared`: `Shared::of` compares the generation's revision
    // and its scope's nonce; `of_query`: the one interner of its `Solved`, see `OfQuery`), so
    // whatever it gives back was made by that database as it still is.
    unsafe { mem::transmute::<&'a Erased, &'a GlobalCache<DbInterner<'db>>>(cache) }
}

/// Which goals a shared cache holds: those solved in one database, for one crate (whose lang
/// items and recursion limit every interner of it read when it was made), with or without
/// region constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Scope {
    nonce: Nonce,
    krate: Option<Crate>,
    regions: bool,
}

/// The value of one key (for the solver, the goals of one scope or query) in one state
/// (revision). Never changed once published but through the value's own `&self` methods and
/// its use mark; dropped, and its slot reused, only through its table's limbo.
struct Generation<K, S, T> {
    key: K,
    state: S,
    /// Set by a use, cleared by a sweep for a slot passing over it: of a full window, the
    /// first not used since the last sweep is given away (second chance).
    referenced: AtomicBool,
    value: T,
}

impl<K, S, T> Generation<K, S, T> {
    #[inline]
    fn touch(&self) {
        // A load first: a hit writes nothing but once per sweep.
        if !self.referenced.load(Ordering::Relaxed) {
            self.referenced.store(true, Ordering::Relaxed);
        }
    }
}

/// How many scopes are kept, `1 << SHARED_BITS`: two (with and without regions) for each crate
/// being checked, in every database alive. A scope given away is only solved again.
const SHARED_BITS: u32 = 14;

/// How many slots from the one a key hashes to it may be kept in.
const PROBE: usize = 16;

const EMPTY: u64 = 0;

/// An open-addressed table from a key to its value in the present state, shared by every
/// thread, of a fixed size. A lookup is up to `PROBE` loads from `hints` (two cache lines),
/// then one pointer load and the generation's key and state; nothing is written but the use
/// mark. A generation is replaced by a compare-and-swap of its pointer, once per key and
/// state. Two replaces of one slot racing may leave it a stale hint: lookups then miss it and
/// the key is made again in another slot, until the slot is next replaced. That costs a
/// solve, never an answer.
struct Shared<'p, K, S, T> {
    /// A hash of the key each slot holds, 0 for none, so a probe loads one word; the
    /// generation says which key it is.
    hints: Box<[AtomicU64]>,
    generations: Box<[AtomicPtr<Generation<K, S, T>>]>,
    /// Where generations are made, each published from [`Pooled::into_raw`].
    pool: &'p Pool<Generation<K, S, T>>,
    /// Locked only to make or replace a generation, for its field updates: a slot taken from
    /// the cache, the swap, the retired one's grace and the limbo's binary search. Every new
    /// interner of every worker takes it, so nothing slow runs under it: the value is made, the
    /// reclaim slots scanned ([`Horizon::now`]) and the passed generations dropped outside.
    /// Taken through [`blaze_graph::wait::lock`]: a contended one blocks the thread (an OS
    /// wait in `parking_lot`, not a fiber's park) inside a `wait` span naming [`LOCK`].
    slots: Mutex<Slots<'p, Generation<K, S, T>>>,
}

/// A table's way into its pool: the cache it makes generations from, and the limbo holding
/// those replaced until no task can still be reading them, then giving their slots back.
struct Slots<'p, G> {
    cache: Cache<'p, G>,
    limbo: Limbo<'p, G>,
}

/// How many generations a table's pool holds: its slots, and those replaced that wait for
/// their grace (see `pool`).
const POOLED: u32 = 1 << 20;

/// The generations of a table, tagged `Infer`; reserved, none committed until made.
fn pool<G>() -> Pool<G> {
    Pool::new(POOLED, Tag::Infer)
        .unwrap_or_else(|error| panic!("the trait solver's cache: {error}"))
}

/// How many generations past their grace a `replace` takes out of its table's limbo, to drop
/// once it has let go of the lock: more than the one it retires, so the limbo drains.
const PASSED: usize = 64;

/// What a wait for a table's lock is said to wait on.
const LOCK: &str = "the trait solver's cache table";

/// Checks, in a debug build, that a holder of a table's lock scanned no reclaim slot since it
/// took the lock and the count was `scans`: T4 (v2-selfbuild2.md) was that scan, once per
/// step of the limbo's binary search, under the lock every new interner takes.
#[inline]
fn held_for_field_updates(scans: u64) {
    debug_assert_eq!(
        blaze_reclaim::scans(),
        scans,
        "a replace scanned the reclaim slots while holding the lock of {LOCK}: take the \
         `Horizon` before locking"
    );
}

/// A slot and the generation it was seen to hold.
type Held<K, S, T> = (usize, *mut Generation<K, S, T>);

/// A shared generation: the search graph's cache and the global-goal table, which answers
/// root goals before they are canonicalized (`global_goals`).
#[derive(Default)]
struct SharedCaches {
    search: Erased,
    goals: crate::next_solver::global_goals::Goals,
}

#[expect(clippy::disallowed_types, reason = "the process's one shared solver generation pool: the shared table allocates all its generations here")]
static SHARED_GENERATIONS: std::sync::LazyLock<Pool<Generation<Scope, View, SharedCaches>>> =
    std::sync::LazyLock::new(pool);

#[expect(clippy::disallowed_types, reason = "the process's one shared solver cache table: all threads share its generations")]
static SHARED: std::sync::LazyLock<Shared<'static, Scope, View, SharedCaches>> =
    std::sync::LazyLock::new(|| Shared::new(SHARED_BITS, std::sync::LazyLock::force(&SHARED_GENERATIONS)));

/// Runs `f` on the shared cache of `krate` (and `regions`) in `db`, as it is now.
#[inline]
pub(super) fn shared<'db, R>(
    db: &'db Db,
    krate: Option<Crate>,
    regions: bool,
    f: impl FnOnce(&GlobalCache<DbInterner<'db>>) -> R,
) -> R {
    let (nonce, revision) = db.nonce_and_revision();
    // Nested in the task the executor runs this in, so it writes nothing but a thread-local
    // count.
    let task = blaze_reclaim::enter();
    f(unerase(&SHARED.of(&task, Scope { nonce, krate, regions }, revision).search))
}

/// Runs `f` on the global-goal table of `krate` (and `regions`) in `db`, as it is now.
#[inline]
pub(super) fn global_goals<R>(
    db: &Db,
    krate: Option<Crate>,
    regions: bool,
    f: impl FnOnce(&crate::next_solver::global_goals::Goals) -> R,
) -> R {
    let (nonce, revision) = db.nonce_and_revision();
    let task = blaze_reclaim::enter();
    f(&SHARED.of(&task, Scope { nonce, krate, regions }, revision).goals)
}

/// A key of a [`Shared`] table.
trait Key: Copy + Eq + Send + Sync + 'static {
    /// Never `EMPTY`, and the same for equal keys; `window` spreads it over the table.
    fn hint(self) -> u64;
}

impl Key for Scope {
    #[inline]
    fn hint(self) -> u64 {
        FixedState.hash_one(self) | 1
    }
}

impl Key for Solved {
    /// A `Solved` is already a number of its own, so it is its hint, unhashed.
    #[inline]
    fn hint(self) -> u64 {
        (self.0 << 1) | 1
    }
}

impl<'p, K, S, T> Shared<'p, K, S, T>
where
    K: Key,
    S: Copy + Eq + Send + Sync + 'static,
    T: Default + Send + Sync + 'static,
{
    fn new(bits: u32, pool: &'p Pool<Generation<K, S, T>>) -> Shared<'p, K, S, T> {
        let len = 1usize << bits;
        Shared {
            // alloc: setup, once per table and process.
            hints: (0..len).map(|_| AtomicU64::new(EMPTY)).collect(),
            // alloc: setup, once per table and process.
            generations: (0..len).map(|_| AtomicPtr::new(ptr::null_mut())).collect(),
            pool,
            slots: Mutex::new(Slots {
                cache: pool.cache(),
                limbo: Limbo::new(pool)
                    .unwrap_or_else(|error| panic!("the trait solver's cache: {error}")),
            }),
        }
    }

    /// The value of `key` in `state`, a new one if there is none, good while `task` is. The
    /// helpers below hand out what they load only to this, so only through it.
    fn of<'g>(&'g self, _task: &'g blaze_reclaim::Guard, key: K, state: S) -> &'g T {
        let hint = key.hint();
        'search: loop {
            for at in window(hint, self.generations.len()) {
                if self.hints[at].load(Ordering::Acquire) != hint {
                    continue;
                }
                let held = self.generations[at].load(Ordering::Acquire);
                let Some(generation) = self.get(held) else { continue };
                if generation.key != key {
                    continue;
                }
                if generation.state == state {
                    generation.touch();
                    return &generation.value;
                }
                // The first use in a new state: what the old value holds may be untrue now
                // (for the solver: its goals read what may have changed since).
                match self.replace(at, held, hint, key, state) {
                    Ok(made) => return made,
                    Err(Some(won)) => return won,
                    Err(None) => continue 'search,
                }
            }
            let (at, held) = match self.victim(hint, key, state) {
                Ok(found) => return found,
                Err(slot) => slot,
            };
            match self.replace(at, held, hint, key, state) {
                Ok(made) | Err(Some(made)) => return made,
                // Another thread gave the slot to another key or state first: look again.
                Err(None) => {}
            }
        }
    }

    /// The generation `held` points to, if any. Called only by `of` and what it calls (and the
    /// tests, under a guard of their own), on a pointer loaded under its guard, and good while
    /// that guard is.
    #[inline]
    fn get(&self, held: *mut Generation<K, S, T>) -> Option<&Generation<K, S, T>> {
        // SAFETY: a generation is dropped and its slot reused only by the table's limbo, after
        // a grace started once it was unlinked from its slot; the caller's guard was entered before `held` was loaded, so
        // that grace has not passed while the guard lives, and `of` ties what it returns to it.
        unsafe { held.as_ref() }
    }

    /// The slot a key hashing to `hint` is given, and what it holds: an empty one, else the
    /// first not used since a sweep last passed over it, clearing the mark of each one passed
    /// over. Or the value of `key` in `state`, if a thread made it and has not yet published
    /// its hint.
    #[cold]
    fn victim(&self, hint: u64, key: K, state: S) -> Result<&T, Held<K, S, T>> {
        let len = self.generations.len();
        let (mut empty, mut first) = (None, None);
        for at in window(hint, len) {
            let held = self.generations[at].load(Ordering::Acquire);
            let Some(generation) = self.get(held) else {
                empty.get_or_insert(at);
                continue;
            };
            if generation.key == key && generation.state == state {
                generation.touch();
                return Ok(&generation.value);
            }
            first.get_or_insert((at, held));
        }
        if let Some(at) = empty {
            return Err((at, ptr::null_mut()));
        }
        for _ in 0..2 {
            for at in window(hint, len) {
                let held = self.generations[at].load(Ordering::Acquire);
                let Some(generation) = self.get(held) else {
                    return Err((at, ptr::null_mut()));
                };
                if !generation.referenced.load(Ordering::Relaxed) {
                    return Err((at, held));
                }
                generation.referenced.store(false, Ordering::Relaxed);
            }
        }
        // Every slot was used again while being swept: the first one goes.
        Err(first.expect("a window has a slot"))
    }

    /// Puts a new generation of `key` (hashing to `hint`) in slot `at` if it still holds
    /// `held`, retiring `held`. When another thread changed the slot first, says what it put
    /// there if that is the value of `key` in `state`, as two threads making one key at once
    /// both pick the same slot.
    #[cold]
    fn replace(
        &self,
        at: usize,
        held: *mut Generation<K, S, T>,
        hint: u64,
        key: K,
        state: S,
    ) -> Result<&T, Option<&T>> {
        // The slow parts before the lock: the value made, and the one scan of every reclaim slot
        // (a line per thread and per parked task) that says which retired generations passed.
        // The scan only where this replace will retire a generation, and so take passed ones:
        // never into an empty slot, nor into one another thread has changed already (a swap
        // that loses after this load still pays one).
        let retiring = match NonNull::new(held) {
            None => None,
            Some(held) => match self.generations[at].load(Ordering::Acquire) {
                now if now == held.as_ptr() => Some((held, Horizon::now())),
                won => return Err(self.won(won, key, state)),
            },
        };
        let generation =
            Generation { key, state, referenced: AtomicBool::new(true), value: T::default() };
        let mut slots = blaze_graph::wait::lock(&self.slots, LOCK);
        let scans = blaze_reclaim::scans();
        let made = slots.cache.alloc(generation).into_raw();
        if let Err(won) = self.generations[at].compare_exchange(
            held,
            made.as_ptr(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: `made` came from `into_raw` of this pool just now and was never published.
            slots.cache.recycle(unsafe { Pooled::from_raw(self.pool, made) });
            held_for_field_updates(scans);
            drop(slots);
            return Err(self.won(won, key, state));
        }
        self.hints[at].store(hint, Ordering::Release);
        let passed = retiring.map(|(held, horizon)| {
            // SAFETY: every generation in a slot was published from `into_raw` of this table's
            // pool, and the swap above unlinked `held`: one swap alone can, so this is the one
            // place that takes it back; the limbo lets it go only after every task that may
            // have loaded it has ended.
            slots.limbo.retire(unsafe { Pooled::from_raw(self.pool, held) });
            slots.limbo.take_passed::<PASSED>(horizon)
        });
        held_for_field_updates(scans);
        drop(slots);
        // The generations past their grace are dropped with the lock let go: a crate's cache is
        // hundreds of megabytes of maps, and every other thread's first use of a scope waits on
        // the lock (`a_retired_generation_is_dropped_outside_the_tables_lock`).
        drop(passed);
        // SAFETY: published above; dropped only by the limbo, after the caller's guard is
        // dropped (see `of`).
        Ok(unsafe { &(*made.as_ptr()).value })
    }

    /// What another thread put in a slot first (`won`), if it is the value of `key` in `state`:
    /// two threads making one key at once both pick the same slot.
    fn won(&self, won: *mut Generation<K, S, T>, key: K, state: S) -> Option<&T> {
        self.get(won).filter(|won| won.key == key && won.state == state).map(|won| &won.value)
    }

    /// Unlinks every generation, and drops those past their grace with the lock let go, as
    /// `replace` does.
    fn clear(&self) {
        let mut slots = blaze_graph::wait::lock(&self.slots, LOCK);
        for (hint, slot) in self.hints.iter().zip(&self.generations) {
            hint.store(EMPTY, Ordering::Release);
            if let Some(held) = NonNull::new(slot.swap(ptr::null_mut(), Ordering::AcqRel)) {
                // SAFETY: as in `replace`: the swap unlinked it, and only one swap can.
                slots.limbo.retire(unsafe { Pooled::from_raw(self.pool, held) });
            }
        }
        drop(slots);
        // Each batch taken moves the limbo's head past it, so this ends once the limbo holds
        // none past its grace: at most its length over `PASSED` rounds.
        loop {
            let horizon = Horizon::now();
            let passed =
                blaze_graph::wait::lock(&self.slots, LOCK).limbo.take_passed::<PASSED>(horizon);
            if passed.is_empty() {
                break;
            }
            drop(passed);
        }
    }
}

impl<K, S, T> Drop for Shared<'_, K, S, T> {
    fn drop(&mut self) {
        for slot in &mut self.generations {
            if let Some(held) = NonNull::new(mem::replace(slot.get_mut(), ptr::null_mut())) {
                // SAFETY: `&mut self`: no reader is left; each generation is in one slot and was
                // published from `into_raw` of this table's pool.
                drop(unsafe { Pooled::from_raw(self.pool, held) });
            }
        }
        // `slots` drops next: the limbo drops what it holds, its owner being gone.
    }
}

/// The slots a key hashing to `hash` may be in, out of `len`, the one it hashes to first.
fn window(hash: u64, len: usize) -> impl Iterator<Item = usize> {
    let mask = len - 1;
    let start = (hash.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - len.trailing_zeros())) as usize;
    (0..PROBE.min(len)).map(move |step| (start + step) & mask)
}

/// Names the query-scoped cache of one interner. A goal whose reads were not captured is
/// only good for where it was solved: what was read to solve it was noted for the query that
/// asked alone, and another query handed the answer would not be run again when an impl
/// changes. An interner does not leave the query that made it, so those goals are kept for it
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Solved(u64);

/// How many numbers a thread takes from the process-wide count at once, so making an
/// interner touches no shared line but once per block.
const SOLVED_BLOCK: u64 = 1 << 12;

impl Solved {
    pub(super) fn new() -> Solved {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        thread_local! {
            /// The next number of this thread's block, and the end of the block.
            static BLOCK: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
        }
        BLOCK.with(|block| {
            let (mut next, mut end) = block.get();
            if next == end {
                next = NEXT.fetch_add(SOLVED_BLOCK, Ordering::Relaxed);
                end = next + SOLVED_BLOCK;
            }
            block.set((next + 1, end));
            Solved(next)
        })
    }
}

/// The interners whose goals are kept, `1 << SLOT_BITS` of them: the queries under way on
/// every thread, as a query that solves makes one and asks other queries that make theirs.
const SLOT_BITS: u32 = 9;

/// The cache of one interner. Its database and revision are those the interner was made in,
/// for as long as it is used: a `Solved` is made with its interner and used only through it,
/// and the interner's `&'db` borrow of the database keeps the revision from advancing
/// (a new view takes `&mut Storage`). So a lookup reads neither; debug builds check.
#[derive(Default)]
struct OfQuery {
    #[cfg(debug_assertions)]
    #[expect(clippy::disallowed_types, reason = "the database and revision, set once per interner cache: not global")]
    state: std::sync::OnceLock<(Nonce, View)>,
    cache: Erased,
}

#[expect(clippy::disallowed_types, reason = "the process's one query solver generation pool: the query table allocates all its generations here")]
static OF_QUERY_GENERATIONS: std::sync::LazyLock<Pool<Generation<Solved, (), OfQuery>>> =
    std::sync::LazyLock::new(pool);

#[expect(clippy::disallowed_types, reason = "the process's one query solver cache table: queries can move between threads")]
static OF_QUERY: std::sync::LazyLock<Shared<'static, Solved, (), OfQuery>> =
    std::sync::LazyLock::new(|| Shared::new(SLOT_BITS, std::sync::LazyLock::force(&OF_QUERY_GENERATIONS)));

// A generation's header is one line; the rest is the search graph's cache.
const _: () = assert!(
    size_of::<Generation<Scope, View, SharedCaches>>() - size_of::<SharedCaches>() <= 64
        && size_of::<Generation<Solved, (), OfQuery>>() - size_of::<Erased>() <= 64
);

/// Runs `f` on the cache of the interner `solved` alone.
#[inline]
pub(super) fn of_query<'db, R>(
    db: &'db Db,
    solved: Solved,
    f: impl FnOnce(&GlobalCache<DbInterner<'db>>) -> R,
) -> R {
    let task = blaze_reclaim::enter();
    let of_query = OF_QUERY.of(&task, solved, ());
    #[cfg(debug_assertions)]
    {
        let state = db.nonce_and_revision();
        assert!(
            *of_query.state.get_or_init(|| state) == state,
            "an interner's solver cache was used in another database or revision"
        );
    }
    #[cfg(not(debug_assertions))]
    let _ = db;
    f(unerase(&of_query.cache))
}

/// Clears the trait solver's caches, of every query and thread.
///
/// Should be called before getting memory usage estimations, as the solver cache
/// is per-revision and usually should be excluded from estimations.
pub fn clear_tls_solver_cache() {
    OF_QUERY.clear();
    SHARED.clear();
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use parking_lot::Mutex;

    use super::{EMPTY, Key, SOLVED_BLOCK, Shared, Solved, pool};

    type Goals = HashMap<u32, u32>;

    /// A shared value: the goals of one key and state.
    type SharedGoals = Mutex<Goals>;

    fn key() -> Solved {
        Solved::new()
    }

    fn address<T>(value: &T) -> usize {
        value as *const T as usize
    }

    /// A table of a pool of its own, leaked so the table is `'static` as the process's are.
    fn table<T: Default + Send + Sync + 'static>(bits: u32) -> Shared<'static, Solved, u32, T> {
        // alloc: a test's pool, once a test.
        Shared::new(bits, Box::leak(Box::new(pool())))
    }

    /// How many slots hold a generation of `key`, read without the table's lookup (which
    /// would mark or make it).
    fn holding(table: &Shared<'_, Solved, u32, SharedGoals>, key: Solved) -> usize {
        let _task = blaze_reclaim::enter();
        table
            .generations
            .iter()
            .filter(|slot| {
                table.get(slot.load(Ordering::Acquire)).is_some_and(|it| it.key == key)
            })
            .count()
    }

    #[test]
    fn solved_numbers_are_unique_across_threads() {
        let per_thread = 3 * SOLVED_BLOCK as usize + 7;
        let seen: Vec<Vec<Solved>> = thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| scope.spawn(move || (0..per_thread).map(|_| Solved::new()).collect()))
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        let all: HashSet<Solved> = seen.iter().flatten().copied().collect();
        assert_eq!(all.len(), 8 * per_thread);
        // Within a block a thread counts up by one.
        for numbers in &seen {
            let runs = numbers.windows(2).filter(|it| it[1].0 != it[0].0 + 1).count();
            assert!(runs <= numbers.len() / SOLVED_BLOCK as usize + 1, "{runs}");
        }
    }

    #[test]
    fn a_solved_hint_is_never_empty_and_names_one_key() {
        let keys: Vec<Solved> = (0..3 * SOLVED_BLOCK).map(|_| key()).collect();
        let hints: HashSet<u64> = keys.iter().map(|it| it.hint()).collect();
        assert_eq!(hints.len(), keys.len());
        assert!(!hints.contains(&EMPTY));
        assert_eq!(Solved(0).hint(), Solved(0).hint());
    }

    #[test]
    fn a_hit_allocates_nothing() {
        let table = table::<SharedGoals>(4);
        let task = blaze_reclaim::enter();
        let query = key();
        table.of(&task, query, 0);
        blaze_alloc_count::assert_at_most(0, "a hit in the solver cache", || {
            for _ in 0..100 {
                table.of(&task, query, 0);
            }
        });
    }

    #[test]
    fn a_miss_into_an_empty_slot_allocates_nothing() {
        let table = table::<SharedGoals>(4);
        let task = blaze_reclaim::enter();
        let query = key();
        blaze_alloc_count::assert_at_most(0, "a miss into an empty slot", || {
            table.of(&task, query, 0);
        });
    }

    /// A replace takes a slot of the pool and hands the old generation to the limbo, which
    /// gives its slot back once its grace has passed: no allocation, warm or waiting.
    #[test]
    fn a_warm_replace_allocates_nothing() {
        let table = table::<SharedGoals>(4);
        let query = key();
        let batch = blaze_mem::pool::BATCH as u32;
        // The first batch is carved and the limbo's column committed; this thread's reclaim
        // slot is claimed.
        for state in 0..2 * batch {
            let task = blaze_reclaim::enter();
            table.of(&task, query, state);
        }
        blaze_alloc_count::assert_at_most(0, "a replace in a new state", || {
            for state in 2 * batch..64 * batch {
                let task = blaze_reclaim::enter();
                table.of(&task, query, state);
            }
        });
        assert_eq!(holding(&table, query), 1);
    }

    #[test]
    fn a_goal_solved_in_one_query_is_not_reused_by_another() {
        let table = table::<SharedGoals>(4);
        let task = blaze_reclaim::enter();
        let (one, other) = (key(), key());
        table.of(&task, one, 0).lock().insert(7, 1);
        assert_eq!(table.of(&task, other, 0).lock().get(&7), None);
        assert_eq!(table.of(&task, one, 0).lock().get(&7), Some(&1));
    }

    #[test]
    fn a_cache_filled_on_one_thread_serves_the_same_query_on_another() {
        let table = table::<SharedGoals>(4);
        let query = key();
        thread::scope(|scope| {
            scope
                .spawn(|| {
                    let task = blaze_reclaim::enter();
                    table.of(&task, query, 0).lock().insert(7, 1);
                })
                .join()
                .unwrap();
            let found = scope.spawn(|| {
                let task = blaze_reclaim::enter();
                table.of(&task, query, 0).lock().get(&7).copied()
            });
            assert_eq!(found.join().unwrap(), Some(1));
        });
    }

    #[test]
    fn every_thread_gets_the_one_value_of_a_key_and_state() {
        let table = table::<SharedGoals>(4);
        let (query, other) = (key(), key());
        let start = Barrier::new(8);
        let seen: Vec<usize> = thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let task = blaze_reclaim::enter();
                        start.wait();
                        address(table.of(&task, query, 0))
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert!(seen.iter().all(|&it| it == seen[0]), "{seen:?}");
        assert_eq!(holding(&table, query), 1);
        let task = blaze_reclaim::enter();
        assert_eq!(address(table.of(&task, query, 0)), seen[0]);
        assert_ne!(address(table.of(&task, other, 0)), seen[0]);
    }

    #[test]
    fn a_new_state_gets_a_new_value() {
        let table = table::<SharedGoals>(4);
        let task = blaze_reclaim::enter();
        let query = key();
        table.of(&task, query, 0).lock().insert(1, 1);
        assert_eq!(table.of(&task, query, 1).lock().get(&1), None);
        assert_eq!(table.of(&task, query, 1).lock().len(), 0);
        // The old state's value is gone, not kept beside the new one.
        assert_eq!(table.of(&task, query, 0).lock().get(&1), None);
        assert_eq!(holding(&table, query), 1);
    }

    #[test]
    fn a_full_window_gives_away_a_value_not_used_since_the_last_sweep() {
        // Every slot is in every window of a 16-slot table.
        let table = table::<SharedGoals>(4);
        let task = blaze_reclaim::enter();
        let queries: Vec<Solved> = (0..16).map(|_| key()).collect();
        for &query in &queries {
            table.of(&task, query, 0).lock().insert(7, 1);
        }
        let (sixteenth, seventeenth) = (key(), key());
        table.of(&task, sixteenth, 0);
        let kept: Vec<Solved> =
            queries.iter().copied().filter(|&it| holding(&table, it) == 1).collect();
        assert_eq!(kept.len(), 15);
        // That sweep cleared every mark; all but one of the queries left are used again.
        let idle = kept[0];
        for &query in &kept[1..] {
            assert_eq!(table.of(&task, query, 0).lock().get(&7), Some(&1));
        }
        table.of(&task, seventeenth, 0);
        assert_eq!(holding(&table, idle), 0);
        for &query in kept[1..].iter().chain([&sixteenth, &seventeenth]) {
            assert_eq!(holding(&table, query), 1);
        }
    }

    #[test]
    fn clear_unlinks_every_value() {
        let table = table::<SharedGoals>(4);
        let task = blaze_reclaim::enter();
        let queries: Vec<Solved> = (0..8).map(|_| key()).collect();
        for &query in &queries {
            table.of(&task, query, 0).lock().insert(1, 1);
        }
        table.clear();
        assert!(table.hints.iter().all(|it| it.load(Ordering::Relaxed) == EMPTY));
        for &query in &queries {
            assert_eq!(table.of(&task, query, 0).lock().get(&1), None);
        }
    }

    /// Counts its own drop once marked.
    #[derive(Default)]
    struct Marked(std::sync::atomic::AtomicBool);

    static MARKED_DROPPED: AtomicUsize = AtomicUsize::new(0);

    impl Drop for Marked {
        fn drop(&mut self) {
            if self.0.load(Ordering::Relaxed) {
                MARKED_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn a_replaced_value_is_freed_only_after_its_readers_left_their_task() {
        let table = table::<Marked>(4);
        let (one, two, three) = (key(), key(), key());
        let (held, go) = (Barrier::new(2), Barrier::new(2));
        thread::scope(|scope| {
            scope.spawn(|| {
                let task = blaze_reclaim::enter();
                let old = table.of(&task, one, 0);
                old.0.store(true, Ordering::Relaxed);
                held.wait();
                go.wait();
                // Replaced, and the limbo collected since, but this task still reads it.
                assert!(old.0.load(Ordering::Relaxed));
                assert_eq!(MARKED_DROPPED.load(Ordering::Relaxed), 0);
            });
            held.wait();
            {
                let task = blaze_reclaim::enter();
                table.of(&task, one, 1);
                table.of(&task, two, 0);
                table.of(&task, two, 1);
            }
            go.wait();
        });
        // The reader left: a later retire collects it (other tests' tasks may hold it a while).
        for state in 0..10_000 {
            if MARKED_DROPPED.load(Ordering::Relaxed) == 1 {
                break;
            }
            let task = blaze_reclaim::enter();
            table.of(&task, three, state);
            drop(task);
            thread::yield_now();
        }
        assert_eq!(MARKED_DROPPED.load(Ordering::Relaxed), 1);
    }

    /// Once armed, its drop waits for another thread to make a value of its table: what a
    /// crate's solver cache, hundreds of megabytes of maps, makes every other thread do while
    /// it drops.
    #[derive(Default)]
    struct SlowDrop(std::sync::atomic::AtomicBool);

    static SLOW_DROPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static SLOW_OTHER_MADE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Rounds a wait of this test yields before it gives up: a drop under the table's lock
    /// leaves the other thread parked on it, so the drop's wait fails here, naming it.
    const SLOW_ROUNDS: u32 = 1 << 22;

    /// Yields until `done`, at most `SLOW_ROUNDS` rounds; whether it was.
    fn yield_until(done: impl Fn() -> bool) -> bool {
        for _ in 0..SLOW_ROUNDS {
            if done() {
                return true;
            }
            thread::yield_now();
        }
        done()
    }

    impl Drop for SlowDrop {
        fn drop(&mut self) {
            if !self.0.load(Ordering::Relaxed) {
                return;
            }
            SLOW_DROPPING.store(true, Ordering::Release);
            assert!(
                yield_until(|| SLOW_OTHER_MADE.load(Ordering::Acquire)),
                "a retired generation was dropped under the table's lock: another thread's first \
                 use of a key waited on the drop"
            );
        }
    }

    /// The selfbuild2 test run's panic (`a wait for a SpinLock ... spun and yielded 16777216
    /// rounds`, making tokio-macros): a new revision retires every crate's shared cache, and
    /// the replace that found them past their grace dropped them all under the table's spin
    /// lock, while every other thread's first use of a scope spun on it.
    #[test]
    fn a_retired_generation_is_dropped_outside_the_tables_lock() {
        let table = table::<SlowDrop>(4);
        let (dropped, other) = (key(), key());
        {
            let task = blaze_reclaim::enter();
            table.of(&task, dropped, 0).0.store(true, Ordering::Relaxed);
        }
        thread::scope(|scope| {
            scope.spawn(|| {
                assert!(yield_until(|| SLOW_DROPPING.load(Ordering::Acquire)), "the armed value was never dropped");
                let task = blaze_reclaim::enter();
                table.of(&task, other, 0);
                SLOW_OTHER_MADE.store(true, Ordering::Release);
            });
            // Each state retires the last one's generation and drops those past their grace:
            // the armed one, once no task of when it was retired is left (other tests' tasks
            // may hold it a while).
            for state in 1..10_000 {
                if SLOW_DROPPING.load(Ordering::Acquire) {
                    break;
                }
                let task = blaze_reclaim::enter();
                table.of(&task, dropped, state);
                drop(task);
                thread::yield_now();
            }
        });
        assert!(SLOW_OTHER_MADE.load(Ordering::Acquire));
    }

    /// Runs the test `name` of this module alone, in a process of its own (this test binary,
    /// asked for that one test with [`CHILD`] set), and fails with its output when it fails:
    /// for a test that leaves the process changed for every test after it.
    fn in_own_process(name: &str) {
        let path = module_path!().split_once("::").map_or(module_path!(), |(_, rest)| rest);
        let exe = std::env::current_exe().expect("the test binary's path");
        let out = std::process::Command::new(&exe)
            .args([&format!("{path}::{name}"), "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap_or_else(|why| panic!("running {} for `{name}`: {why}", exe.display()));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        println!("{stdout}{stderr}");
        // libtest's summary for exactly this one test: a filter that matched none says
        // "0 passed" and exits 0. Under `blaze test` the fork server takes its socket's variable
        // out of the environment before any test runs, and a run attached in process loses
        // `BLAZE_INPROC` at its start hook, so this child is libtest's `main` either way.
        assert!(
            out.status.success() && stdout.contains("test result: ok. 1 passed;"),
            "`{name}` in its own process: {}\n{stdout}\n{stderr}",
            out.status
        );
    }

    /// Set in the process [`in_own_process`] runs a test in.
    const CHILD: &str = "BLAZE_TLS_CACHE_TEST_CHILD";

    /// T4 (v2-selfbuild2.md; the backtrace in /tmp/sb-printer/logs/spin-bt.txt): building
    /// rustc_type_ir and hir-def, three workers' first use of an interner's cache waited on the
    /// query table's lock for `1 << 24` spin rounds and panicked, while the rest of the build
    /// went on taking that lock. Its holders were never stuck: each replace held it for
    /// `take_passed`, whose binary search over the limbo asked `Grace::passed`, a scan of every
    /// reclaim slot, at each step, and with a parked task pinning an old epoch no grace passes,
    /// so the limbo only grows. A slot per parked task and a step per doubling of the limbo:
    /// the lock was held for milliseconds by every new interner of 64 workers, and a waiter
    /// that kept losing it spun out its bound.
    ///
    /// This parks `PARKED` tasks (as fibers waiting on keys carry their reclaim slots), fills
    /// the limbo, and has `WORKERS` threads make a new key each, as every query that solves
    /// does, until they made `KEYS` or `RUN` is over, whichever comes first. Every one must get
    /// the table each time within `LONGEST`. The first parked task pins the epoch, so nothing
    /// retired passes: `KEYS` keeps the run inside the pool however fast the machine is. Run in
    /// a process of its own: the slots it adds stay in every later scan of the process.
    #[test]
    fn a_replace_holds_the_tables_lock_for_no_scan_of_the_parked_tasks() {
        if std::env::var_os(CHILD).is_none() {
            in_own_process("a_replace_holds_the_tables_lock_for_no_scan_of_the_parked_tasks");
            return;
        }
        a_replace_holds_the_tables_lock_for_no_scan_of_the_parked_tasks_here();
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "a stress test's wall-clock bound and its measure of the longest wait"
    )]
    fn a_replace_holds_the_tables_lock_for_no_scan_of_the_parked_tasks_here() {
        use std::time::{Duration, Instant};

        const PARKED: usize = 1 << 16;
        const RETIRED: u32 = 1 << 16;
        const WORKERS: usize = 16;
        const KEYS: usize = 400_000;
        const RUN: Duration = Duration::from_secs(20);
        const LONGEST: Duration = Duration::from_secs(1);
        const BITS: u32 = 9;
        // Every key made retires a generation, and none passes: the filled limbo, the keys, the
        // table's slots and the workers' last ones fit the pool, so the run never meets
        // "pool full".
        const _: () = assert!(RETIRED as usize + KEYS + (1 << BITS) + WORKERS < super::POOLED as usize);

        let table = table::<std::sync::atomic::AtomicBool>(BITS);
        // One task parked first pins the epoch every grace after it waits for.
        let pin = |count: usize| -> Vec<std::num::NonZeroUsize> {
            (0..count)
                .map(|_| {
                    let task = blaze_reclaim::enter();
                    let parked = blaze_reclaim::park().expect("a thread in a task parks it");
                    // The task went with its slot: this thread is in none, so no guard ends it.
                    std::mem::forget(task);
                    parked
                })
                .collect()
        };
        let mut parked = pin(1);
        // A limbo long enough for its binary search to take 16 steps.
        let filled = Instant::now();
        let query = key();
        for state in 0..RETIRED {
            let task = blaze_reclaim::enter();
            table.of(&task, query, state);
        }
        parked.extend(pin(PARKED - 1));
        let filled = filled.elapsed();
        println!("filled the limbo with {RETIRED} and parked {PARKED} tasks in {filled:?}");

        let started = Instant::now();
        let until = started + RUN;
        let made = AtomicUsize::new(0);
        let longest: Vec<Duration> = thread::scope(|scope| {
            let workers: Vec<_> = (0..WORKERS)
                .map(|_| {
                    let (table, made) = (&table, &made);
                    scope.spawn(move || {
                        let mut longest = Duration::ZERO;
                        while Instant::now() < until && made.fetch_add(1, Ordering::Relaxed) < KEYS {
                            let task = blaze_reclaim::enter();
                            let asked = Instant::now();
                            table.of(&task, key(), 0);
                            longest = longest.max(asked.elapsed());
                        }
                        longest
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|it| it.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
                .collect()
        });
        for slot in parked {
            blaze_reclaim::end_parked(slot);
        }
        let worst = longest.iter().max().copied().unwrap_or_default();
        let made = made.into_inner().min(KEYS);
        let took = started.elapsed();
        println!("{made} new keys by {WORKERS} threads in {took:?}; the longest wait {worst:?}");
        assert!(worst < LONGEST, "a new key waited {worst:?} for the table, past {LONGEST:?}");
    }

    /// The cache before the table: per thread, the last `KEPT` queries' goals, all dropped
    /// when the state changes.
    struct PerThread {
        caches: Vec<(Solved, Goals)>,
        state: u32,
    }

    impl PerThread {
        const KEPT: usize = 32;

        fn of(&mut self, query: Solved, state: u32) -> &mut Goals {
            if self.state != state {
                self.caches.clear();
                self.state = state;
            }
            match self.caches.iter().rposition(|it| it.0 == query) {
                Some(at) => {
                    let found = self.caches.remove(at);
                    self.caches.push(found);
                }
                None => {
                    if self.caches.len() == Self::KEPT {
                        self.caches.remove(0);
                    }
                    self.caches.push((query, Goals::new()));
                }
            }
            &mut self.caches.last_mut().unwrap().1
        }
    }

    /// What solving `goal` for `query` in `state` gives: different for each, so an answer
    /// handed to another query or state is seen.
    fn solve(query: Solved, state: u32, goal: u32) -> u32 {
        (query.0 as u32).wrapping_mul(0x9E37_79B9) ^ state.wrapping_mul(0x85EB_CA6B) ^ goal
    }

    /// Answers a goal through `goals`, solving it when it is not there.
    fn answer(goals: &mut Goals, query: Solved, state: u32, goal: u32) -> u32 {
        *goals.entry(goal).or_insert_with(|| solve(query, state, goal))
    }

    #[test]
    fn answers_equal_those_of_a_cache_per_thread() {
        // A small table, so values are given away while in use; states change under readers
        // (which the solver never does, as a revision needs `&mut` of the database), so
        // replacing and freeing race with lookups.
        let table = table::<SharedGoals>(5);
        let queries: Vec<Solved> = (0..96).map(|_| key()).collect();
        let shared_hits = AtomicUsize::new(0);
        thread::scope(|scope| {
            for worker in 0..8u64 {
                let (table, queries, shared_hits) = (&table, &queries, &shared_hits);
                scope.spawn(move || {
                    let mut per_thread = PerThread { caches: Vec::new(), state: 0 };
                    let mut rng = blaze_ids::rng::SplitMix64::new(0x2545_F491_4F6C_DD1D ^ (worker + 1));
                    for step in 0..20_000u32 {
                        let task = blaze_reclaim::enter();
                        // Each query is taken up by every thread, as a task moves; the state
                        // changes twice, as revisions do.
                        let query = queries[rng.index(queries.len())];
                        let state = step / 8_000;
                        let goal = rng.below(64) as u32;
                        let truth = solve(query, state, goal);
                        let shared = {
                            let mut goals = table.of(&task, query, state).lock();
                            if goals.contains_key(&goal) {
                                shared_hits.fetch_add(1, Ordering::Relaxed);
                            }
                            answer(&mut goals, query, state, goal)
                        };
                        let local = answer(per_thread.of(query, state), query, state, goal);
                        assert_eq!(shared, truth, "{query:?} {state} {goal}");
                        assert_eq!(shared, local, "{query:?} {state} {goal}");
                    }
                });
            }
        });
        assert!(shared_hits.into_inner() > 0, "the table never answered from its cache");
    }
}
