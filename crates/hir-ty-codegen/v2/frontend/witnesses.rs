//! What a database's describes and counts read below `Durability::HIGH`, kept for the life of
//! its kept walks: the "fact -> the rows that read it" index of
//! docs/v2/plan/design/end-state/codegen.md, in the query engine's node keys.
//!
//! A describe was a memo of the query database (`described_memo`, a count `counted_memo`)
//! until 2026-11-12, and a cold build of tempo held 3.87 M of its rows (2.22 GiB), about
//! 8.1 GiB of their values and every read of each instance's monomorphization as their edges,
//! for two things only: a describe the next build's walk could take without running it, and
//! the keys of the changed set. A describe is now run by the task that visits it, as any
//! value read once, and each run leaves here the keys of what it read below `HIGH`
//! (`blaze_query::db::witnessed`). The changed set is the engine's changed keys since a view
//! (`blaze_query::db::changed_keys_since`) looked up in those postings. Reads at `HIGH` (a
//! library's sources) need none: a write of one opens a new durable view, which no kept walk
//! is patched across and under which this table is made anew ([`of_in`]).

use std::{
    alloc::Global,
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use base_db::Db;
use blaze_codegen::{backend::Counted, plan::Origin};
use blaze_core::lock::Lock;
use blaze_hash::{HashMap, SharedMap};
use blaze_ids::Hasher;
use blaze_query::db::View;

use super::{Facing, HirInstance};

/// The domain of a [`digest`]: what a describe or count's value is by its `Debug` text.
const DIGEST_DOMAIN: &str = "blaze v2 witnessed value 1";

/// How many locks the postings are split over, by the read key's hash: describes note on
/// every core at once.
const SHARDS: usize = 64;

/// What one database's describes and counts read below `HIGH`, for the life of its kept
/// walks, in its owner's [`super::Walks`].
pub(super) struct Witnesses {
    /// The database's durable view (`Storage::durable_view`) when made: a write of an input
    /// at `HIGH` makes the table anew ([`of_in`]).
    durable: u64,
    /// The database's view (`Storage::view`) when made: no changed set is named from before
    /// it, as no describe run before it left its reads here.
    pub(super) since: u64,
    /// Each describe by its key: the instance, who asks and what it read of the front end.
    pub(super) describes: Table<(HirInstance, Origin, Facing)>,
    /// Each count ([`Counted`]) by its instance.
    pub(super) counts: Table<HirInstance>,
    /// What each count noted in [`Self::counts`] was, given again while it [`Table::holds`].
    pub(super) counted: SharedMap<HirInstance, Counted>,
}

impl Witnesses {
    fn new(durable: u64, since: u64) -> Witnesses {
        Witnesses {
            durable,
            since,
            describes: Table::new(since),
            counts: Table::new(since),
            counted: SharedMap::default(),
        }
    }

    /// How many postings both tables hold.
    pub(super) fn postings(&self) -> u64 {
        self.describes.posted.load(Ordering::Relaxed) + self.counts.posted.load(Ordering::Relaxed)
    }
}

/// What a describe or count's last run was: where its key is, whether its reads are all
/// here, whether one changed since, and what it made.
#[derive(Clone, Copy)]
struct Noted {
    /// Its key's place in [`Table::keys`], which its postings name.
    at: u32,
    /// Every read of its last run is posted (`blaze_query::db::Witness::complete`) and it
    /// read nothing beside the database. Not: its caller asks it again each build.
    complete: bool,
    /// A read posted for it changed since its last run ([`Table::refresh`]).
    dirty: bool,
    /// The [`digest`] of what its last run made.
    value: [u8; 16],
    /// Its place is in [`Table::incomplete`]: a run of it was noted not complete.
    listed: bool,
}

/// Runs, keyed by `K`, and the keys of what each read below `HIGH`. Postings are added and
/// never taken out while the table lives: a stale one names a key once more than needed,
/// never one less.
pub(super) struct Table<K> {
    noted: SharedMap<K, Noted>,
    /// The places ([`Noted::at`]) posted under each read key, split by the read key's hash.
    by_read: [Lock<HashMap<u64, Vec<u32>, Global>>; SHARDS],
    /// Each key by its place, so a posting is four bytes.
    keys: Lock<Vec<K>>,
    /// The places of keys a run of which was noted not complete ([`Noted::listed`]).
    incomplete: Lock<Vec<u32>>,
    /// The view the dirty flags are up to date in ([`Self::refresh`]).
    seen: AtomicU64,
    /// Held by the one refresh of a view.
    refreshing: Lock<()>,
    /// How many postings were added.
    posted: AtomicU64,
    /// The bug a test plants ([`Plant`]).
    #[cfg(test)]
    pub(crate) plant: std::sync::atomic::AtomicU8,
}

/// The bugs a test plants in a [`Table`], each the oracle's control. Planted only by a
/// test; a build's table has none.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Plant {
    None = 0,
    /// A note keeps the dirty flag its key had.
    KeepsDirty = 1,
    /// A refresh looks in every shard of the postings but the first.
    SkipsShard = 2,
    /// `holds` compares no value.
    IgnoresValue = 3,
    /// A note posts none of its reads.
    DropsReads = 4,
}

/// The shard of the postings a read key is in.
fn shard(read: u64) -> usize {
    (read.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize
}

impl<K: Hash + Eq + Clone + Send + Sync> Table<K> {
    fn new(seen: u64) -> Table<K> {
        Table {
            noted: SharedMap::default(),
            by_read: std::array::from_fn(|_| {
                Lock::new("codegen.frontend.witnesses.reads", HashMap::new_in(Global))
            }),
            keys: Lock::new("codegen.frontend.witnesses.keys", Vec::new()),
            incomplete: Lock::new("codegen.frontend.witnesses.incomplete", Vec::new()),
            seen: AtomicU64::new(seen),
            refreshing: Lock::new("codegen.frontend.witnesses.refreshing", ()),
            posted: AtomicU64::new(0),
            #[cfg(test)]
            plant: std::sync::atomic::AtomicU8::new(Plant::None as u8),
        }
    }

    #[cfg(test)]
    fn planted(&self, plant: Plant) -> bool {
        self.plant.load(Ordering::Relaxed) == plant as u8
    }

    #[cfg(not(test))]
    fn planted(&self, _: Plant) -> bool {
        false
    }

    /// How many keys are noted.
    pub(super) fn len(&self) -> usize {
        self.noted.len()
    }

    /// The place of `key`, given one if it has none, and what it was noted as before.
    fn place(&self, key: &K) -> (u32, Option<Noted>) {
        if let Some(was) = self.noted.pin().get(key).copied() {
            return (was.at, Some(was));
        }
        let mut keys = self.keys.lock();
        // Looked up again under the lock: one place a key, whoever notes it first.
        if let Some(was) = self.noted.pin().get(key).copied() {
            return (was.at, Some(was));
        }
        let at = u32::try_from(keys.len()).unwrap_or_else(|_| {
            panic!("more than {} describes or counts noted in one database's witnesses", u32::MAX)
        });
        keys.push(key.clone()); // clone: the key, kept once by its place
        // Dirty until its note is made: nothing holds it before then.
        let placed = Noted { at, complete: false, dirty: true, value: [0; 16], listed: false };
        // clone: the key, the map's own
        _ = self.noted.pin().insert(key.clone(), placed);
        (at, None)
    }

    /// Notes a run of `key` that read `reads` (below `HIGH`, each once; put in shard order
    /// here) and made what [`digest`]s to `value`. It holds from now until a read of it
    /// changes ([`Self::refresh`]).
    pub(super) fn note(&self, key: &K, reads: &mut [u64], complete: bool, value: [u8; 16]) {
        let (at, was) = self.place(key);
        reads.sort_unstable_by_key(|it| shard(*it));
        let mut posted = 0;
        if !self.planted(Plant::DropsReads) {
            for run in reads.chunk_by(|a, b| shard(*a) == shard(*b)) {
                let mut postings = self.by_read[shard(run[0])].lock();
                for &read in run.iter() {
                    // alloc: one list a read key, kept while the table lives
                    let places = postings.entry(read).or_insert_with(Vec::new);
                    // A run that read it again after no other's run did is posted once.
                    if places.last() != Some(&at) {
                        places.push(at); // alloc: one posting a read below `HIGH`
                        posted += 1;
                    }
                }
            }
        }
        self.posted.fetch_add(posted, Ordering::Relaxed);
        // Listed once, and never taken out: a key whose run is not complete is always listed.
        let listed = was.is_some_and(|it| it.listed) || !complete;
        if !complete && !was.is_some_and(|it| it.listed) {
            self.incomplete.lock().push(at); // alloc: one place a key first noted incomplete
        }
        let dirty = match self.planted(Plant::KeepsDirty) {
            true => was.is_some_and(|it| it.dirty),
            false => false,
        };
        // clone: the key, the map's own
        _ = self.noted.pin().insert(key.clone(), Noted { at, complete, dirty, value, listed });
    }

    /// Brings the dirty flags up to `db`'s view, once a view: each key posted under a key the
    /// engine names changed since the last refresh turns dirty. Every run noted since read in
    /// a view at or after the last refresh, so one whose read changed after it is named; one
    /// whose read changed before it is named too, which only costs it one run.
    pub(super) fn refresh(&self, db: &Db) -> Result<(), String> {
        let now = blaze_query::db::Database::storage(db).view().as_u64();
        if self.seen.load(Ordering::Acquire) == now {
            return Ok(());
        }
        let _refreshing = self.refreshing.lock();
        let seen = self.seen.load(Ordering::Acquire);
        if seen == now {
            return Ok(());
        }
        let mut changed = Vec::new(); // alloc: once a view, the engine's changed keys
        blaze_query::db::changed_keys_since(db, View::from_u64(seen), &mut changed).map_err(
            |why| {
                // alloc: an error
                format!(
                    "the query engine names no changed keys since view {seen} for the \
                     witnesses of the describes and counts: {why}"
                )
            },
        )?;
        self.mark(&changed);
        self.seen.store(now, Ordering::Release);
        Ok(())
    }

    /// Turns dirty every key posted under one of `changed`.
    fn mark(&self, changed: &[u64]) {
        let mut places = Vec::new(); // alloc: once a view, the places the changed keys name
        self.posted_under(changed, &mut places);
        let keys = self.keys.lock();
        let noted = self.noted.pin();
        for at in places {
            // clone: the key by value, as the map takes it
            _ = noted.update(keys[at as usize].clone(), |was| Noted { dirty: true, ..*was });
        }
    }

    /// Every place posted under one of `changed`, sorted, each once.
    fn posted_under(&self, changed: &[u64], places: &mut Vec<u32>) {
        for &read in changed {
            let at = shard(read);
            if at == 0 && self.planted(Plant::SkipsShard) {
                continue;
            }
            if let Some(posted) = self.by_read[at].lock().get(&read) {
                places.extend_from_slice(posted);
            }
        }
        places.sort_unstable();
        places.dedup();
    }

    /// Whether `key`'s last run made what [`digest`]s to `value` and nothing it read changed
    /// since, as of the last [`Self::refresh`]: one lock-free look.
    pub(super) fn holds(&self, key: &K, value: [u8; 16]) -> bool {
        let ignores = self.planted(Plant::IgnoresValue);
        self.noted
            .pin()
            .get(key)
            .is_some_and(|it| it.complete && !it.dirty && (ignores || it.value == value))
    }

    /// Whether `key` is noted and its last run's reads are not all here: asked each build.
    pub(super) fn each(&self, key: &K) -> bool {
        self.noted.pin().get(key).is_some_and(|it| !it.complete)
    }

    /// Every key posted under one of `changed`, each once, into `out`.
    pub(super) fn named(&self, changed: &[u64], out: &mut Vec<K>) {
        let mut places = Vec::new(); // alloc: once a build, the places the changed keys name
        self.posted_under(changed, &mut places);
        let keys = self.keys.lock();
        // clone: the keys named, the caller's
        out.extend(places.iter().map(|at| keys[*at as usize].clone()));
    }

    /// Every key whose last run was not complete, each once, into `out`.
    pub(super) fn incomplete(&self, out: &mut Vec<K>) {
        let places = self.incomplete.lock();
        let keys = self.keys.lock();
        let noted = self.noted.pin();
        for at in places.iter() {
            let key = &keys[*at as usize];
            if noted.get(key).is_some_and(|it| !it.complete) {
                out.push(key.clone()); // clone: the key named, the caller's
            }
        }
    }

    /// How many postings were added.
    #[cfg(test)]
    fn posted(&self) -> u64 {
        self.posted.load(Ordering::Relaxed)
    }
}

/// What a describe's or count's value is, by its `Debug` text, as the describe record's
/// stage hashes a `Debug` (`described::Stage::of`): 16 bytes of the text's content ID, the
/// text streamed into the hasher, never made.
pub(super) fn digest<T: std::fmt::Debug>(value: &T) -> [u8; 16] {
    struct Streamed(Hasher);
    impl std::fmt::Write for Streamed {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0.update(text.as_bytes());
            Ok(())
        }
    }
    let mut streamed = Streamed(Hasher::new(DIGEST_DOMAIN));
    // A `Debug` of a derived type writes no error of its own.
    std::fmt::write(&mut streamed, format_args!("{value:?}"))
        .unwrap_or_else(|_| panic!("a `Debug` of a witnessed value failed to write"));
    let id = streamed.0.finish();
    let mut out = [0; 16];
    out.copy_from_slice(&id.0[..16]);
    out
}

#[expect(
    blaze::process_static,
    reason = "a per-thread read buffer taken out for each run, uncapped: it keeps the most reads one run made; the run should own it"
)]
thread_local! {
    /// The read keys of the witnessed run on this thread: taken out for the run and put back
    /// after, so no borrow of it is held across the run's waits, and a run nested in it (on
    /// a fiber the thread runs while this one waits) takes its own.
    // alloc: one buffer a thread, as large as the most reads one run made
    static READS: std::cell::Cell<Vec<u64>> = const { std::cell::Cell::new(Vec::new()) };
}

/// Runs `run` witnessed (`blaze_query::db::witnessed`), then `note` with what it made, the
/// keys it read below `HIGH` and whether those are all of its reads.
pub(super) fn witnessed<R>(
    db: &Db,
    run: impl FnOnce() -> R,
    note: impl FnOnce(&R, &mut [u64], bool),
) -> R {
    let mut reads = READS.take();
    reads.clear();
    let (made, witness) = blaze_query::db::witnessed(db, &mut reads, run);
    note(&made, &mut reads, witness.complete);
    reads.clear();
    READS.set(reads);
    made
}

/// The witnesses of `db` kept in `slot` (its owner's, [`super::Walks`]; or none, for a front
/// end whose builds keep nothing): the slot's while no input at `HIGH` was written since they
/// were made, else made anew, empty, from its current view, and kept in the slot. No process
/// static holds any: they go with the database's owner.
pub(super) fn of_in(slot: &mut Option<Arc<Witnesses>>, db: &Db) -> Arc<Witnesses> {
    let storage = blaze_query::db::Database::storage(db);
    let durable = storage.durable_view().as_u64();
    if let Some(kept) = slot
        && kept.durable == durable
    {
        return kept.clone(); // clone: a reference count, once a build
    }
    // alloc: once per owner and write of an input at `HIGH`, an empty table
    let made = Arc::new(Witnesses::new(durable, storage.view().as_u64()));
    *slot = Some(made.clone()); // clone: a reference count
    made
}

#[cfg(test)]
mod tests {
    use hegel::{Hegel, Settings, TestCase, generators as gs};

    use super::{Plant, Table};

    /// One step of a model run.
    #[derive(Debug, Clone)]
    enum Step {
        /// Key `key` ran, read `reads`, made `value`; its reads are all here or not.
        Note { key: u32, reads: Vec<u64>, complete: bool, value: u8 },
        /// These read keys changed, and a refresh saw them.
        Change(Vec<u64>),
    }

    /// The model of one key since its last run.
    #[derive(Clone, Default)]
    struct Model {
        /// What its last run read.
        reads: Vec<u64>,
        /// Every read any of its runs posted: a stale posting names it too.
        posted: Vec<u64>,
        complete: bool,
        value: u8,
        /// A read of its last run changed since.
        changed: bool,
        /// A read any of its runs posted changed since its last run.
        dirty: bool,
    }

    fn value(it: u8) -> [u8; 16] {
        super::digest(&it)
    }

    /// Runs `steps` on a table with `plant` and checks the oracle after each.
    fn check(steps: &[Step], plant: Plant) -> Result<(), String> {
        let table: Table<u32> = Table::new(0);
        table.plant.store(plant as u8, std::sync::atomic::Ordering::Relaxed);
        let mut model: Vec<Option<Model>> = vec![None; 8];
        // Every read key changed since the start, which `named` is asked of.
        let mut all_changed: Vec<u64> = Vec::new();
        for (n, step) in steps.iter().enumerate() {
            match step {
                Step::Note { key, reads, complete, value: made } => {
                    // clone: the test's reads, which `note` sorts
                    let mut sorted = reads.clone();
                    table.note(key, &mut sorted, *complete, value(*made));
                    let slot = &mut model[*key as usize];
                    let mut posted = slot.take().map(|it| it.posted).unwrap_or_default();
                    posted.extend(reads);
                    *slot = Some(Model {
                        reads: reads.clone(), // clone: the model's own
                        posted,
                        complete: *complete,
                        value: *made,
                        changed: false,
                        dirty: false,
                    });
                }
                Step::Change(reads) => {
                    table.mark(reads);
                    all_changed.extend(reads);
                    for it in model.iter_mut().flatten() {
                        it.changed |= it.reads.iter().any(|read| reads.contains(read));
                        it.dirty |= it.posted.iter().any(|read| reads.contains(read));
                    }
                }
            }
            let mut named = Vec::new();
            table.named(&all_changed, &mut named);
            for (key, it) in model.iter().enumerate() {
                let Some(it) = it else { continue };
                let key = key as u32;
                let (got, should) = (table.holds(&key, value(it.value)), it.complete && !it.dirty);
                if got != should {
                    return Err(format!(
                        "after step {n} ({step:?}) key {key} holds its value: {got}, the model: {should}"
                    ));
                }
                if it.changed && got {
                    return Err(format!("after step {n} key {key}, a read of which changed, holds"));
                }
                if table.holds(&key, value(it.value.wrapping_add(1))) {
                    return Err(format!("after step {n} key {key} holds a value it never made"));
                }
                if it.changed && !named.contains(&key) {
                    return Err(format!(
                        "after step {n} key {key}, a read of which changed, is not named"
                    ));
                }
                if table.each(&key) == it.complete {
                    return Err(format!("after step {n} key {key}'s each-build is not its model's"));
                }
            }
        }
        Ok(())
    }

    fn steps(tc: &TestCase) -> Vec<Step> {
        let count: usize = tc.draw(gs::integers().min_value(1).max_value(24));
        let read = || tc.draw(gs::integers::<u64>().max_value(40));
        (0..count)
            .map(|_| match tc.draw(gs::integers::<u8>().max_value(2)) {
                0 => {
                    let changed = tc.draw(gs::integers::<u8>().max_value(3));
                    Step::Change((0..changed).map(|_| read()).collect())
                }
                _ => {
                    let reads = tc.draw(gs::integers::<u8>().max_value(5));
                    Step::Note {
                        key: tc.draw(gs::integers::<u32>().max_value(7)),
                        reads: (0..reads).map(|_| read()).collect(),
                        complete: tc.draw(gs::integers::<u8>().max_value(4)) != 0,
                        value: tc.draw(gs::integers::<u8>().max_value(2)),
                    }
                }
            })
            .collect()
    }

    /// A read key in the first shard, which the planted [`Plant::SkipsShard`] passes over.
    fn of_first_shard() -> u64 {
        (0..).find(|it| super::shard(*it) == 0).expect("some key is in the first shard")
    }

    /// The invariant (a model table): after any notes and refreshes, a key one of whose reads
    /// changed since its last note is named and does not hold; a complete key none of whose
    /// posted reads changed since its last note holds its last value; no key holds a value its
    /// last run did not make; a key is asked each build exactly when its last run was not
    /// complete. Controls: a note that keeps the dirty flag (a key run again after a change
    /// never holds again), a refresh that skips a shard (a key whose read is in it holds after
    /// the read changed).
    #[test]
    fn no_changed_read_is_held_or_left_unnamed() {
        let cases =
            std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(300);
        Hegel::new(|tc: TestCase| {
            if let Err(why) = check(&steps(&tc), Plant::None) {
                panic!("{why}");
            }
        })
        .settings(Settings::new().test_cases(cases).database(None))
        .run();

        let read = of_first_shard();
        let rerun = [
            Step::Note { key: 1, reads: vec![read], complete: true, value: 0 },
            Step::Change(vec![read]),
            Step::Note { key: 1, reads: vec![read], complete: true, value: 1 },
        ];
        assert!(check(&rerun, Plant::None).is_ok());
        assert!(check(&rerun, Plant::KeepsDirty).is_err(), "a note that keeps dirty passed");
        let changed = [
            Step::Note { key: 2, reads: vec![read], complete: true, value: 0 },
            Step::Change(vec![read]),
        ];
        assert!(check(&changed, Plant::None).is_ok());
        assert!(check(&changed, Plant::SkipsShard).is_err(), "a refresh that skips a shard passed");
    }

    /// A run that reads a key again, with no other run between, posts it once.
    #[test]
    fn a_rerun_posts_a_read_once() {
        let table: Table<u32> = Table::new(0);
        table.note(&1, &mut [5, 6], true, [0; 16]);
        table.note(&1, &mut [6, 5], true, [0; 16]);
        assert_eq!(table.posted(), 2);
    }
}
