use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use blaze_ids::ContentId;

use super::{Column, Edit, Free, Ids, Map, Rows};
use crate::{QueryKey, QueryKind};

fn key(n: u64) -> QueryKey {
    QueryKey { kind: QueryKind(1), input: ContentId::of("blaze v2 db table test 1", &n.to_le_bytes()) }
}

/// Waits for what a map unlinked to be freed: other tests' threads may still be in tasks that
/// began before it was unlinked.
fn freed<V: Send + Sync + 'static>(map: &Map<V>) {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        map.sweep();
        if map.held() == 0 {
            return;
        }
        assert!(Instant::now() < until, "what the map unlinked was never freed");
        std::thread::yield_now();
    }
}

/// A shard whose entries are removed and put back rehashes into the tables it outgrew, kept
/// past their grace: once settled, the cycle allocates nothing.
#[test]
fn a_settled_map_rehashes_without_allocating() {
    let map: Map<u64> = Map::new();
    let cycle = |map: &Map<u64>| {
        {
            let task = blaze_reclaim::enter();
            for n in 0..1024 {
                map.insert(&task, key(n), n);
            }
            map.retain(&task, |_, _| false);
        }
        freed(map);
    };
    for _ in 0..3 {
        cycle(&map);
    }
    blaze_alloc_count::assert_at_most(0, "a settled map's remove-and-insert cycle", || cycle(&map));
    let task = blaze_reclaim::enter();
    assert_eq!(map.get(&task, &key(7)), None);
}

/// An edit of every entry split among the plane's workers by shard leaves the map as one
/// thread's does, and reports each edit made once, over random edits of random contents.
#[test]
#[cfg_attr(blaze_check, ignore = "the plane's workers: not the model checker's threads")]
fn a_parallel_edit_of_every_entry_is_the_serial_one() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..8u64 {
        let (serial, parallel): (Map<u64>, Map<u64>) = (Map::new(), Map::new());
        let task = blaze_reclaim::enter();
        for _ in 0..(round + 1) * 700 {
            let (n, value) = (next() % 5000, next());
            serial.insert(&task, key(n), value);
            parallel.insert(&task, key(n), value);
        }
        let salt = next();
        let decide = |value: u64| match (value ^ salt) % 3 {
            0 => Edit::Remove,
            1 => Edit::Replace(value.rotate_left(7)),
            _ => Edit::Keep,
        };
        let mut said = Vec::new();
        serial.edit_all_with(&task, |_, &value| decide(value), |&old, new| said.push((old, new.copied())));
        let heard = parking_lot::Mutex::new(Vec::new());
        parallel.edit_all_parallel(
            Vec::new,
            |_, _, _, &value| decide(value),
            |mine: &mut Vec<(u64, Option<u64>)>, &old, new| mine.push((old, new.copied())),
            |mine| heard.lock().extend(mine),
        );
        let mut heard = heard.into_inner();
        said.sort_unstable();
        heard.sort_unstable();
        assert_eq!(heard, said, "round {round}: the edits each made");
        let contents = |map: &Map<u64>| {
            let mut all = Vec::new();
            map.for_each(&task, |key, &value| all.push((*key, value)));
            all.sort_unstable();
            all
        };
        assert_eq!(contents(&parallel), contents(&serial), "round {round}: what is left");
    }
}

#[test]
#[cfg_attr(blaze_check, ignore = "many threads, long loops: slow under the model checker's shims")]
fn readers_see_every_published_entry_whole_while_tables_grow() {
    let map: Map<(u64, u64)> = Map::new();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for reader in 0..4u64 {
            let (map, done) = (&map, &done);
            scope.spawn(move || {
                while !done.load(Ordering::Acquire) {
                    let task = blaze_reclaim::enter();
                    for n in (reader..8 * 2048).step_by(97) {
                        if let Some(&(a, b)) = map.get(&task, &key(n)) {
                            assert_eq!((a, b), (n, !n), "an entry is read whole or not at all");
                        }
                    }
                }
            });
        }
        let writers: Vec<_> = (0..8u64)
            .map(|writer| {
                let map = &map;
                scope.spawn(move || {
                    for n in writer * 2048..(writer + 1) * 2048 {
                        let task = blaze_reclaim::enter();
                        assert_eq!(map.insert(&task, key(n), (n, !n)), &(n, !n));
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("a writer");
        }
        done.store(true, Ordering::Release);
    });
    let task = blaze_reclaim::enter();
    for n in 0..8 * 2048 {
        assert_eq!(map.get(&task, &key(n)), Some(&(n, !n)));
    }
    let mut count = 0;
    map.for_each(&task, |_, _| count += 1);
    assert_eq!(count, 8 * 2048);
    drop(task);
    freed(&map);
}

#[test]
fn a_replaced_entry_stays_readable_until_its_task_ends() {
    let map: Map<String> = Map::new();
    let task = blaze_reclaim::enter();
    map.insert(&task, key(1), "old".to_owned());
    let old = map.get(&task, &key(1)).expect("inserted");
    map.insert(&task, key(1), "new".to_owned());
    assert_eq!(old, "old", "a reader keeps what it loaded");
    assert_eq!(map.get(&task, &key(1)).map(String::as_str), Some("new"));
    map.sweep();
    assert_eq!(map.held(), 1, "the old entry waits for this task");
    drop(task);
    freed(&map);
}

/// A replaced entry's row is not handed out again while a task that could hold it lives, and
/// is once its grace has passed: entries are rows of one column, never a box each.
#[test]
fn a_retired_row_is_reused_only_after_its_grace() {
    let map: Map<u64> = Map::new();
    let task = blaze_reclaim::enter();
    map.insert(&task, key(1), 1);
    let first = map.row(&task, &key(1)).expect("inserted");
    map.insert(&task, key(1), 2);
    let second = map.row(&task, &key(1)).expect("replaced");
    assert_ne!(first, second, "the old row may still be read");
    map.insert(&task, key(1), 3);
    map.sweep();
    let third = map.row(&task, &key(1)).expect("replaced");
    assert!(third != first && third != second, "rows {first} and {second} wait for this task");
    drop(task);
    freed(&map);

    let task = blaze_reclaim::enter();
    map.insert(&task, key(1), 4);
    let reused = map.row(&task, &key(1)).expect("replaced");
    assert!(reused == first || reused == second, "row {reused} is new, not one past its grace");
    assert_eq!(map.get(&task, &key(1)), Some(&4));
}

#[test]
fn rows_are_one_region_by_index_and_never_move() {
    let rows: Rows<u64> = Rows::new();
    let ids = Ids::new();
    let mut free = Free::default();
    let claimed: Vec<u32> = (0..10_000).map(|_| ids.claim(&mut free)).collect();
    assert_eq!(claimed, (0..10_000).collect::<Vec<u32>>(), "one writer's rows are dense");
    for &row in &claimed {
        // SAFETY: claimed just now by this thread alone and not yet published.
        unsafe { rows.put(row, u64::from(row) * 3) };
    }
    // SAFETY: each row was put above by this thread, and nothing drops it.
    let first = unsafe { rows.get(0) } as *const u64;
    for &row in &claimed {
        // SAFETY: as above.
        let at = unsafe { rows.get(row) };
        assert_eq!(*at, u64::from(row) * 3);
        assert_eq!(at as *const u64, first.wrapping_add(row as usize), "row {row} is base + index");
    }
    // SAFETY: as above.
    assert_eq!(unsafe { rows.get(0) } as *const u64, first, "growing never moved row 0");
}

/// A table read at numbers nobody claimed (the kinds) finds what was written and nothing else.
#[test]
fn a_row_never_written_reads_as_none() {
    let rows: Rows<u64> = Rows::sized(1 << 16);
    // SAFETY: no reader yet; the row holds no value.
    unsafe { rows.put(0x2f0c, 7) };
    // SAFETY: written above by this thread and never dropped.
    let read = |row| unsafe { rows.written(row) }.copied();
    assert_eq!(read(0x2f0c), Some(7));
    assert_eq!(read(0x2f0b), None, "below the row written");
    assert_eq!(read(0xfff0), None, "past the row written");
}

#[test]
#[cfg_attr(blaze_check, ignore = "many threads, long loops: slow under the model checker's shims")]
fn get_or_insert_keeps_the_first_value() {
    let map: Map<u64> = Map::new();
    let task = blaze_reclaim::enter();
    assert_eq!(map.get_or_insert_with(&task, key(1), || 1), &1);
    assert_eq!(map.get_or_insert_with(&task, key(1), || unreachable!("present")), &1);
    let winners: Vec<u64> = std::thread::scope(|scope| {
        let spawned: Vec<_> = (0..8u64)
            .map(|thread| {
                let map = &map;
                scope.spawn(move || *map.get_or_insert_with(&blaze_reclaim::enter(), key(2), || thread))
            })
            .collect();
        spawned.into_iter().map(|it| it.join().expect("a thread")).collect()
    });
    assert!(winners.iter().all(|it| *it == winners[0]), "one value won: {winners:?}");
}

#[test]
fn removed_entries_leave_tombs_reused_and_dropped_by_growth() {
    let map: Map<u64> = Map::new();
    let task = blaze_reclaim::enter();
    for n in 0..1000 {
        map.insert(&task, key(n), n);
    }
    map.retain(&task, |_, value| value % 2 == 0);
    for n in 0..1000 {
        assert_eq!(map.get(&task, &key(n)).copied(), (n % 2 == 0).then_some(n));
    }
    for _ in 0..4 {
        for n in (1..1000).step_by(2) {
            map.insert(&task, key(n), n);
        }
        map.retain(&task, |_, value| value % 2 == 0);
    }
    let mut live = 0;
    map.for_each(&task, |_, value| {
        assert_eq!(value % 2, 0);
        live += 1;
    });
    assert_eq!(live, 500, "the evens, each once, and no odd kept");
    map.edit_all(&task, |_, value| if *value < 10 { Edit::Replace(value + 1000) } else { Edit::Keep });
    assert_eq!(map.get(&task, &key(4)), Some(&1004));
    drop(task);
    freed(&map);
}

#[test]
#[cfg_attr(blaze_check, ignore = "many threads, long loops: slow under the model checker's shims")]
fn an_update_sees_the_entry_it_replaces() {
    let map: Map<Vec<u64>> = Map::new();
    let task = blaze_reclaim::enter();
    std::thread::scope(|scope| {
        for thread in 0..8u64 {
            let map = &map;
            scope.spawn(move || {
                let task = blaze_reclaim::enter();
                for n in 0..100 {
                    map.update(&task, key(0), |old| {
                        let mut new = old.cloned().unwrap_or_default();
                        new.push(thread * 100 + n);
                        Some(new)
                    });
                }
            });
        }
    });
    let mut all = map.get(&task, &key(0)).expect("updated").clone();
    all.sort_unstable();
    assert_eq!(all, (0..800).collect::<Vec<u64>>(), "no update was lost");
    map.update(&task, key(0), |_| None);
    assert_eq!(map.get(&task, &key(0)).map(Vec::len), Some(800), "`None` leaves it");
}

#[test]
#[cfg_attr(blaze_check, ignore = "many threads, long loops: slow under the model checker's shims")]
fn a_column_publishes_takes_and_restores_by_index() {
    let mut column: Column<String> = Column::new();
    let indices: Vec<u32> = std::thread::scope(|scope| {
        let spawned: Vec<_> = (0..4)
            .map(|thread| {
                let column = &column;
                scope.spawn(move || {
                    (0..100).map(|n| column.push(|index| format!("{thread}:{n}:{index}"))).collect::<Vec<_>>()
                })
            })
            .collect();
        spawned.into_iter().flat_map(|it| it.join().expect("a thread")).collect()
    });
    let task = blaze_reclaim::enter();
    for index in &indices {
        assert!(column.get(&task, *index).expect("pushed").ends_with(&format!(":{index}")));
    }
    let taken = column.take(7).expect("pushed");
    assert!(column.get(&task, 7).is_none());
    assert!(column.take(7).is_none(), "taken once");
    drop(task);
    drop(taken);
    let (next, values) = column.take_all();
    assert_eq!((next, values.len()), (400, 399));
    column.restore(next, values, |value| value.rsplit(':').next().and_then(|it| it.parse().ok()).expect("an index"));
    assert_eq!(column.values_mut().count(), 399);
    assert_eq!(column.push(|index| index.to_string()), 400);
}

/// A lookup, hit or miss, allocates nothing.
#[test]
fn a_map_lookup_allocates_nothing() {
    let map: Map<u64> = Map::new();
    let task = blaze_reclaim::enter();
    for n in 0..256 {
        map.insert(&task, key(n), n);
    }
    let found = blaze_alloc_count::assert_at_most(0, "a map lookup", || {
        (0..512).filter(|&n| map.get(&task, &key(n)) == Some(&n)).count()
    });
    assert_eq!(found, 256);
    drop(task);
    freed(&map);
}

/// Model tests (docs/v2/plan/lanes/safety/model.md): each structure from two or three threads
/// under every interleaving and every value its loads may read. What a lock, a
/// read-modify-write or a join orders is checked against a sequential model; what a lone
/// acquire load sees is asserted directly, since it may be older than a store that came
/// earlier in the run when nothing orders the two.
#[cfg(blaze_check)]
mod model {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        },
    };

    use blaze_check::{
        cell::UnsafeCell,
        linear::{History, Spec},
        sync::atomic::AtomicU32,
        thread,
    };
    use blaze_ids::ContentId;

    use crate::{
        QueryKey, QueryKind,
        table::{COLUMN_FIRST, Column, Map},
    };

    /// Entries that fill a shard's first table: the next insert into the shard grows it.
    const FULL: u8 = 8;

    /// Keys of one shard (their first four bytes pick it), each with a home of its own.
    fn key(n: u8) -> QueryKey {
        let mut input = [0; 32];
        input[8] = n;
        QueryKey { kind: QueryKind(1), input: ContentId(input) }
    }

    #[derive(Default)]
    struct Counts {
        made: AtomicUsize,
        dropped: AtomicUsize,
    }

    impl Counts {
        fn balanced(&self) {
            assert_eq!(self.made.load(Relaxed), self.dropped.load(Relaxed), "every value made is dropped, once");
        }
    }

    /// A value the model tracks: the thread that makes it writes it, and each read is checked
    /// against that write, so a read the structure's release and acquire don't order after it is
    /// a data race.
    struct Tracked {
        value: UnsafeCell<u64>,
        counts: Arc<Counts>,
    }

    // SAFETY: `value` is written once, in `Tracked::new` before the value is shared, and only read
    // after; the model reports any read that write does not happen before.
    unsafe impl Sync for Tracked {}

    impl Tracked {
        fn new(counts: &Arc<Counts>, value: u64) -> Tracked {
            counts.made.fetch_add(1, Relaxed);
            let tracked = Tracked { value: UnsafeCell::new(0), counts: counts.clone() };
            // SAFETY: not shared yet: this is the only access.
            tracked.value.with_mut(|it| unsafe { *it = value });
            tracked
        }

        fn get(&self) -> u64 {
            // SAFETY: written only by `new`, before the value was published.
            self.value.with(|it| unsafe { *it })
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.counts.dropped.fetch_add(1, Relaxed);
        }
    }

    /// A map's sequential model.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Entries(BTreeMap<u8, u64>);

    #[derive(Clone, Copy, Debug)]
    enum MapOp {
        Insert(u8, u64),
        GetOrInsert(u8, u64),
        /// Adds to the value (to 0 when there is none); gives the value it replaced.
        Add(u8, u64),
        Get(u8),
    }

    impl Spec for Entries {
        type Op = MapOp;
        type Ret = Option<u64>;

        fn apply(&mut self, op: &MapOp) -> Option<u64> {
            match *op {
                MapOp::Insert(n, value) => {
                    self.0.insert(n, value);
                    Some(value)
                }
                MapOp::GetOrInsert(n, value) => Some(*self.0.entry(n).or_insert(value)),
                MapOp::Add(n, value) => {
                    let old = self.0.get(&n).copied();
                    self.0.insert(n, old.unwrap_or(0) + value);
                    old
                }
                MapOp::Get(n) => self.0.get(&n).copied(),
            }
        }
    }

    struct Writers {
        map: Map<Tracked>,
        history: History<Entries>,
        counts: Arc<Counts>,
    }

    impl Writers {
        fn run(&self, op: MapOp) {
            let task = blaze_reclaim::enter();
            let (map, counts) = (&self.map, &self.counts);
            self.history.record(op, |op| match *op {
                MapOp::Insert(n, value) => Some(map.insert(&task, key(n), Tracked::new(counts, value)).get()),
                MapOp::GetOrInsert(n, value) => {
                    Some(map.get_or_insert_with(&task, key(n), || Tracked::new(counts, value)).get())
                }
                MapOp::Add(n, value) => {
                    let mut replaced = None;
                    map.update(&task, key(n), |old| {
                        replaced = old.map(Tracked::get);
                        Some(Tracked::new(counts, replaced.unwrap_or(0) + value))
                    });
                    replaced
                }
                MapOp::Get(n) => map.get(&task, &key(n)).map(Tracked::get),
            });
        }
    }

    /// Writers of one shard while its table grows: inserts, a racing `get_or_insert_with` (one
    /// value wins, the other is dropped) and an `update` that must see the entry it replaces,
    /// against a map. Each decision is made under the shard's lock, so the history is ordered
    /// by it; the reads after the joins are ordered by them.
    #[test]
    fn model_map_writers_linearize_while_a_shard_grows() {
        blaze_check::model(|| {
            let counts = Arc::new(Counts::default());
            let writers = Arc::new(Writers { map: Map::new(), history: History::new(), counts: counts.clone() });
            let mut init = Entries::default();
            {
                let task = blaze_reclaim::enter();
                for n in 0..FULL {
                    writers.map.insert(&task, key(n), Tracked::new(&counts, 100 + u64::from(n)));
                    init.0.insert(n, 100 + u64::from(n));
                }
            }
            let spawn = |ops: [MapOp; 2]| {
                let writers = writers.clone();
                thread::spawn(move || {
                    for op in ops {
                        writers.run(op);
                    }
                })
            };
            let one = spawn([MapOp::Insert(FULL, 1), MapOp::GetOrInsert(FULL + 1, 2)]);
            let two = spawn([MapOp::Add(FULL, 10), MapOp::GetOrInsert(FULL + 1, 3)]);
            writers.run(MapOp::Insert(0, 4));
            one.join().expect("a writer");
            two.join().expect("a writer");
            for n in [0, FULL, FULL + 1] {
                writers.run(MapOp::Get(n));
            }
            writers.history.check(init);
            drop(writers);
            counts.balanced();
        });
    }

    /// A reader of one shard while a writer grows its table and another thread removes an entry:
    /// an entry nobody touches is found in every table (the copy is published only once
    /// complete), every entry found is read whole (the tracked payload), and after the joins the
    /// map holds exactly what was inserted and kept.
    #[test]
    fn model_map_readers_see_entries_whole_while_a_shard_grows_and_loses_one() {
        blaze_check::model(|| {
            let counts = Arc::new(Counts::default());
            let map = Arc::new(Map::<Tracked>::new());
            {
                let task = blaze_reclaim::enter();
                for n in 0..FULL {
                    map.insert(&task, key(n), Tracked::new(&counts, u64::from(n)));
                }
            }
            let writer = {
                let (map, counts) = (map.clone(), counts.clone());
                thread::spawn(move || {
                    let task = blaze_reclaim::enter();
                    for n in [FULL, FULL + 1] {
                        map.insert(&task, key(n), Tracked::new(&counts, u64::from(n)));
                    }
                })
            };
            let reader = {
                let map = map.clone();
                thread::spawn(move || {
                    let task = blaze_reclaim::enter();
                    assert_eq!(
                        map.get(&task, &key(0)).map(Tracked::get),
                        Some(0),
                        "an entry no writer touches is in every table"
                    );
                    for n in [1, FULL, FULL + 1] {
                        let found = map.get(&task, &key(n)).map(Tracked::get);
                        assert!(found.is_none_or(|it| it == u64::from(n)), "key {n} read as {found:?}");
                    }
                })
            };
            {
                // The one thread here in a `blaze_mem::frame::task` (`edit_all`'s scratch is
                // the OS thread's, which every model thread shares).
                let task = blaze_reclaim::enter();
                map.retain(&task, |key, _| key.input.0[8] != 1);
            }
            writer.join().expect("the writer");
            reader.join().expect("the reader");
            let task = blaze_reclaim::enter();
            for n in 0..FULL + 2 {
                let want = (n != 1).then_some(u64::from(n));
                assert_eq!(map.get(&task, &key(n)).map(Tracked::get), want, "key {n}");
            }
            let mut live = 0;
            map.for_each(&task, |_, _| live += 1);
            assert_eq!(live, usize::from(FULL) + 1, "each entry once, the removed one gone");
            drop(task);
            drop(map);
            counts.balanced();
        });
    }

    /// A column's sequential model: its values by index, a taken one `None`.
    #[derive(Clone, PartialEq, Eq, Hash)]
    struct Values(Vec<Option<u64>>);

    #[derive(Clone, Copy, Debug)]
    enum ColumnOp {
        Push(u64),
        Get(u32),
        Take(u32),
    }

    impl Spec for Values {
        type Op = ColumnOp;
        type Ret = Option<u64>;

        fn apply(&mut self, op: &ColumnOp) -> Option<u64> {
            match *op {
                ColumnOp::Push(value) => {
                    self.0.push(Some(value));
                    Some(self.0.len() as u64 - 1)
                }
                ColumnOp::Get(index) => self.0.get(index as usize).copied().flatten(),
                ColumnOp::Take(index) => self.0.get_mut(index as usize).and_then(Option::take),
            }
        }
    }

    /// Values a first chunk holds but one: the next pushes straddle a chunk no thread has made.
    const PUSHED: u32 = COLUMN_FIRST as u32 - 1;

    fn push(column: &Column<Tracked>, history: &History<Values>, counts: &Arc<Counts>, value: u64) -> u32 {
        let index =
            history.record(ColumnOp::Push(value), |_| Some(u64::from(column.push(|_| Tracked::new(counts, value)))));
        u32::try_from(index.expect("a push gives its index")).expect("a u32 index")
    }

    fn get(column: &Column<Tracked>, history: &History<Values>, index: u32) {
        let task = blaze_reclaim::enter();
        history.record(ColumnOp::Get(index), |_| column.get(&task, index).map(Tracked::get));
    }

    /// Three pushes, two of them into a chunk they race to make, each read back by its own
    /// thread or taken, against a vector: indices are dense and distinct and each value is
    /// read whole. A value handed to another thread with a relaxed store is read as nothing or
    /// whole: only the column's release and acquire order it.
    #[test]
    fn model_column_pushes_publish_whole_values_into_a_new_chunk() {
        blaze_check::model(|| {
            let counts = Arc::new(Counts::default());
            let column = Arc::new(Column::<Tracked>::new());
            let history = Arc::new(History::<Values>::new());
            let mut init = Values(Vec::new());
            for n in 0..u64::from(PUSHED) {
                column.push(|_| Tracked::new(&counts, n));
                init.0.push(Some(n));
            }
            let handed = Arc::new(AtomicU32::new(u32::MAX));
            let getter = {
                let (column, history, counts, handed) = (column.clone(), history.clone(), counts.clone(), handed.clone());
                thread::spawn(move || {
                    let index = push(&column, &history, &counts, 1000);
                    handed.store(index, Relaxed);
                    get(&column, &history, index);
                    index
                })
            };
            let taker = {
                let (column, history, counts) = (column.clone(), history.clone(), counts.clone());
                thread::spawn(move || {
                    let index = push(&column, &history, &counts, 2000);
                    let mut taken = None;
                    history.record(ColumnOp::Take(index), |_| {
                        taken = column.take(index);
                        taken.as_deref().map(Tracked::get)
                    });
                    // Held, as the database's limbo holds it, until no reader can have it.
                    (index, taken)
                })
            };
            let mine = push(&column, &history, &counts, 3000);
            let index = handed.load(Relaxed);
            if index != u32::MAX {
                let task = blaze_reclaim::enter();
                let found = column.get(&task, index).map(Tracked::get);
                assert!(found.is_none_or(|it| it == 1000), "index {index} read as {found:?}");
            }
            let theirs = getter.join().expect("the getter");
            let (taken_at, taken) = taker.join().expect("the taker");
            for index in [theirs, taken_at, mine] {
                get(&column, &history, index);
            }
            history.check(init);
            drop(taken);
            drop(column);
            counts.balanced();
        });
    }
}
