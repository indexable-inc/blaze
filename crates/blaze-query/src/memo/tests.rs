use std::{
    sync::Barrier,
    time::{Duration, Instant},
};

use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Fingerprint};

use super::{MemoId, Slot, Store, value_row};
use crate::{Edge, QueryKey, QueryKind, Stored, segment::Span, table::EdgeRecord};

const SCHEMA: SchemaHash = SchemaHash(ContentId([0x57; 32]));
static ON_DISK: &[u8] = b"the bytes a cold pack holds";

fn key(n: u64) -> QueryKey {
    QueryKey { kind: QueryKind(0x2f00), input: ContentId::of("blaze v2 db memo test 1", &n.to_le_bytes()) }
}

fn edges(n: u64) -> Vec<EdgeRecord> {
    (0..3).map(|i| EdgeRecord::new(key(1000 + n * 3 + i), Fingerprint(u128::from(n) << 64 | u128::from(i)))).collect()
}

fn read(records: &[EdgeRecord]) -> Vec<Edge> {
    records.iter().map(EdgeRecord::edge).collect()
}

/// Waits until `done`, collecting: other tests' threads may still be in tasks that began
/// before what the store let go of was unlinked.
fn until(store: &Store, what: &str, mut done: impl FnMut(&Store) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let mut check = Vec::new();
        store.to_check(&mut check);
        store.checked(&check, |_| false);
        store.sweep();
        if done(store) {
            return;
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::yield_now();
    }
}

/// A cold write names the value it copied by its row and that row's generation: once the row
/// is reused for another value of the memo, the stale word is refused.
#[test]
fn a_value_locations_generation_refuses_a_stale_swap() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    let memo = store.make(key(0), Fingerprint(7), code, &[], b"first");
    let task = blaze_reclaim::enter();
    let (was, bytes) = store.in_memory(&task, memo).expect("in memory");
    assert_eq!(bytes, b"first");
    assert!(store.evict(&task, memo));
    assert_eq!(store.stored(&task, memo), Stored::Evicted);
    assert!(!store.evict(&task, memo), "evicted once");
    drop(task);
    until(&store, "the evicted value's grace", |store| store.held() == 0);

    let task = blaze_reclaim::enter();
    store.refill(&task, memo, b"again");
    let (now, bytes) = store.in_memory(&task, memo).expect("refilled");
    assert_eq!(bytes, b"again");
    assert_eq!(value_row(now), value_row(was), "the row past its grace is reused");
    assert_ne!(now, was, "under a generation of its own");
    // SAFETY: a static outlives the store.
    let stale = unsafe { store.to_disk(&task, memo, was, ON_DISK, ContentId([2; 32])) };
    assert!(!stale, "the word names a value the memo no longer has");
    assert_eq!(store.stored(&task, memo), Stored::Memory);
    assert_eq!(store.bytes(&task, memo), Some(&b"again"[..]));
    // SAFETY: as above.
    let fresh = unsafe { store.to_disk(&task, memo, now, ON_DISK, ContentId([2; 32])) };
    assert!(fresh);
    assert_eq!(store.stored(&task, memo), Stored::Disk(ContentId([2; 32])));
    assert_eq!(store.bytes(&task, memo), Some(ON_DISK));
    assert!(!store.evict(&task, memo), "a mapped value is not in memory");
}

/// A hit reads a value's bytes from the memo's own line: the copy there follows every swap of
/// the value's word, and is never used once the word names another value.
#[test]
fn a_memos_line_holds_where_its_current_value_is() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    let memo = store.make(key(1), Fingerprint(8), code, &[], b"made");
    let task = blaze_reclaim::enter();
    let inline = |store: &Store| {
        store.hot(&task, memo).inline().map(|(bytes, len)| {
            // SAFETY: the copy names the value the word names, which this task holds.
            unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec()
        })
    };
    assert_eq!(inline(&store).as_deref(), Some(&b"made"[..]), "set when the memo is made");
    assert!(store.evict(&task, memo));
    assert_eq!(inline(&store), None, "no value, no copy");
    assert_eq!(store.bytes(&task, memo), None);
    store.refill(&task, memo, b"again");
    assert_eq!(inline(&store).as_deref(), Some(&b"again"[..]), "rewritten by the refill's swap");
    let (word, _) = store.in_memory(&task, memo).expect("refilled");
    // SAFETY: a static outlives the store.
    assert!(unsafe { store.to_disk(&task, memo, word, ON_DISK, ContentId([2; 32])) });
    assert_eq!(inline(&store).as_deref(), Some(ON_DISK), "and by the cold write's");
    assert_eq!(store.bytes(&task, memo), Some(ON_DISK));
}

/// Each task's edges are copied once, with its memo, into its thread's edge segment: one
/// thread's memos are adjacent ranges of one segment, and two threads write two segments.
#[test]
#[cfg_attr(blaze_check, ignore = "many threads, long loops: slow under the model checker's shims")]
fn a_tasks_edges_are_one_range_of_its_threads_segment() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    let barrier = Barrier::new(2);
    let made: Vec<Vec<MemoId>> = std::thread::scope(|scope| {
        let spawned: Vec<_> = (0..2u64)
            .map(|thread| {
                let (store, barrier) = (&store, &barrier);
                scope.spawn(move || {
                    // Both threads alive at once, so each holds a lane of its own.
                    barrier.wait();
                    (0..50)
                        .map(|n| {
                            let n = thread * 50 + n;
                            store.make(key(n), Fingerprint(u128::from(n)), code, &edges(n), &n.to_le_bytes())
                        })
                        .collect::<Vec<MemoId>>()
                })
            })
            .collect();
        spawned.into_iter().map(|it| it.join().expect("a thread")).collect()
    });
    let task = blaze_reclaim::enter();
    for (thread, memos) in made.iter().enumerate() {
        for (at, &memo) in memos.iter().enumerate() {
            let n = thread as u64 * 50 + at as u64;
            assert_eq!(read(store.edges(&task, memo)), read(&edges(n)));
            assert_eq!(store.bytes(&task, memo), Some(&n.to_le_bytes()[..]));
            assert_eq!(store.key(&task, memo), key(n));
            assert_eq!(store.fingerprint(&task, memo), Fingerprint(u128::from(n)));
        }
        for pair in memos.windows(2) {
            let (a, b) = (store.edges(&task, pair[0]), store.edges(&task, pair[1]));
            assert_eq!(a.as_ptr_range().end, b.as_ptr(), "one thread's memos' edges are adjacent");
        }
    }
    assert_eq!(store.segments(), (2, 2, 0), "one edge and one value segment a thread");
}

/// Memos are freed a segment at a time: a segment stays while one memo in it lives, and is
/// freed whole once none does; a freed memo's row is reused.
#[test]
fn a_segment_is_freed_whole_once_nothing_in_it_is_held() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    // 168 bytes of edges each: the first 16 KiB segment holds 97, the second the rest.
    let memos: Vec<MemoId> = (0..200).map(|n| store.make(key(n), Fingerprint(1), code, &edges(n), &[])).collect();
    assert_eq!(store.segments().0, 2);
    let task = blaze_reclaim::enter();
    let first = store.edges(&task, memos[0]).as_ptr();
    let last_of_first = store.edges(&task, memos[96]).as_ptr();
    assert_eq!(last_of_first, first.wrapping_add(96 * 3), "memo 96 is the first segment's last");
    drop(task);

    let (kept, rest): (Vec<MemoId>, Vec<MemoId>) = memos.iter().partition(|&&memo| memo == memos[96]);
    store.unlinked(&rest);
    until(&store, "freeing all but one memo", |store| store.dying() == 0);
    assert_eq!(store.segments().0, 2, "memo 96 holds the first segment; the second is open");

    store.unlinked(&kept);
    until(&store, "freeing the last memo of the first segment", |store| store.segments().0 == 1);
    assert_eq!(store.dying(), 0);

    let again = store.make(key(0), Fingerprint(1), code, &edges(0), &[]);
    assert!(memos.contains(&again), "memo {again} is a new row, not a freed one");
}

#[test]
fn a_slot_holds_three_memos_in_itself_and_more_in_a_list() {
    let store = Store::new();
    let few = store.slot(None, &[4, 5, 6]);
    assert_eq!(few.memos(), &[4, 5, 6]);
    assert_eq!(few.key(), None);
    assert_eq!(store.segments().2, 0, "three memos are inline");
    let many = store.slot(Some(store.key_bytes(b"the key's bytes")), &[1, 2, 3, 4, 5]);
    assert_eq!(many.memos(), &[1, 2, 3, 4, 5]);
    assert_eq!(many.key(), Some(&b"the key's bytes"[..]));
    let shared = many.share_key();
    drop(many);
    assert_eq!(shared.as_ref().map(Span::bytes), Some(&b"the key's bytes"[..]), "a shared key outlives its slot");
    assert!(Slot::empty().memos().is_empty());
}

/// Reading a memo (its line, value, edges, key and where its value is) allocates nothing.
#[test]
fn a_store_read_allocates_nothing() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    let made = edges(4);
    let memo = store.make(key(4), Fingerprint(4), code, &made, b"read me");
    let task = blaze_reclaim::enter();
    let (fingerprint, inline, bytes, edge_len, at, stored) =
        blaze_alloc_count::assert_at_most(0, "a store read", || {
            (
                store.fingerprint(&task, memo),
                store.hot(&task, memo).inline().map(|(_, len)| len),
                store.bytes(&task, memo),
                store.edges(&task, memo).len(),
                store.key(&task, memo),
                store.stored(&task, memo),
            )
        });
    assert_eq!((fingerprint, inline, bytes, edge_len), (Fingerprint(4), Some(7), Some(&b"read me"[..]), 3));
    assert_eq!((at, stored), (key(4), Stored::Memory));
}

/// Once a thread has its lane, open segments and a block of rows, making a memo copies into
/// them and allocates nothing.
#[test]
fn a_memo_made_on_a_warm_thread_allocates_nothing() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    for n in 0..8 {
        store.make(key(n), Fingerprint(1), code, &edges(n), &n.to_le_bytes());
    }
    let made = edges(8);
    let memo = blaze_alloc_count::assert_at_most(0, "a memo made on a warm thread", || {
        store.make(key(8), Fingerprint(1), code, &made, &8u64.to_le_bytes())
    });
    let task = blaze_reclaim::enter();
    assert_eq!(read(store.edges(&task, memo)), read(&made));
    assert_eq!(store.bytes(&task, memo), Some(&8u64.to_le_bytes()[..]));
}

/// Model tests (docs/v2/plan/lanes/safety/model.md) of the store's lock-free parts: a memo's
/// value word and the inline copy that follows it, and the segments' hold counts. The bytes
/// segments copy in, read and free are tracked (`crate::sync`'s hooks), so a read the atomics
/// don't order after its copy, or before its segment's free, is a finding.
#[cfg(blaze_check)]
mod model {
    use std::sync::{Arc, atomic::Ordering::Relaxed};

    use blaze_check::{
        Builder,
        linear::{History, Spec},
        thread,
    };
    use blaze_ids::{ContentId, Fingerprint};

    use super::{SCHEMA, key};
    use crate::{
        Stored,
        memo::{MemoId, Shared, Store},
        segment::{Segments, Span},
    };

    /// Where a memo's value is: in memory by its version, on disk, or nowhere.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    enum Held {
        Memory(u8),
        Disk,
        Evicted,
    }

    #[derive(Clone, Copy, Debug)]
    enum ValueOp {
        Evict,
        Refill(u8),
        /// Moves the value of this version to disk, if it is still the memo's.
        ToDisk(u8),
        Look,
    }

    impl Spec for Held {
        type Op = ValueOp;
        type Ret = Option<Held>;

        fn apply(&mut self, op: &ValueOp) -> Option<Held> {
            match *op {
                ValueOp::Evict => matches!(self, Held::Memory(_)).then(|| {
                    *self = Held::Evicted;
                    Held::Evicted
                }),
                ValueOp::Refill(version) => {
                    if *self == Held::Evicted {
                        *self = Held::Memory(version);
                    }
                    None
                }
                ValueOp::ToDisk(version) => (*self == Held::Memory(version)).then(|| {
                    *self = Held::Disk;
                    Held::Disk
                }),
                ValueOp::Look => Some(*self),
            }
        }
    }

    const DISK: ContentId = ContentId([9; 32]);
    static ON_DISK: &[u8] = b"dk";

    /// Where the value of `bytes` is, checking it is read whole.
    fn where_is(bytes: &[u8]) -> Held {
        match bytes {
            [b'm', version] => Held::Memory(version - b'0'),
            b"dk" => Held::Disk,
            other => panic!("a value read whole, not {other:?}"),
        }
    }

    fn one_memo(value: &[u8]) -> (Store, MemoId) {
        let store = Store::new();
        let code = store.code(SCHEMA, ContentId([1; 32]));
        let memo = store.make(key(0), Fingerprint(7), code, &[], value);
        (store, memo)
    }

    /// The inline copy names the value the word names: checked once every swap is done.
    fn copy_follows(store: &Store, memo: MemoId) {
        let task = blaze_reclaim::enter();
        let hot = store.hot(&task, memo);
        assert_eq!(
            hot.seen.load(Relaxed),
            hot.value.load(Relaxed),
            "the inline copy names the value the word names once every swap is done"
        );
    }

    struct One {
        store: Store,
        memo: MemoId,
        history: History<Held>,
    }

    impl One {
        fn evict(&self) {
            let task = blaze_reclaim::enter();
            self.history.record(ValueOp::Evict, |_| self.store.evict(&task, self.memo).then_some(Held::Evicted));
        }

        fn refill(&self, version: u8) {
            let task = blaze_reclaim::enter();
            self.history.record(ValueOp::Refill(version), |_| {
                self.store.refill(&task, self.memo, &[b'm', b'0' + version]);
                None
            });
        }

        /// A cold write: the value this thread sees (a lone load, outside the history), then the
        /// swap that moves it to disk if the memo still has that value.
        fn to_disk(&self) {
            let task = blaze_reclaim::enter();
            let Some((was, bytes)) = self.store.in_memory(&task, self.memo) else { return };
            let Held::Memory(version) = where_is(bytes) else { panic!("`Store::in_memory` gave a mapped value") };
            self.history.record(ValueOp::ToDisk(version), |_| {
                // SAFETY: a static outlives the store.
                let moved = unsafe { self.store.to_disk(&task, self.memo, was, ON_DISK, DISK) };
                moved.then_some(Held::Disk)
            });
        }

        fn look(&self) {
            let task = blaze_reclaim::enter();
            self.history.record(ValueOp::Look, |_| {
                let held = self.store.bytes(&task, self.memo).map_or(Held::Evicted, where_is);
                let stored = match held {
                    Held::Memory(_) => Stored::Memory,
                    Held::Disk => Stored::Disk(DISK),
                    Held::Evicted => Stored::Evicted,
                };
                assert_eq!(self.store.stored(&task, self.memo), stored);
                Some(held)
            });
        }
    }

    /// An eviction, a refill and two cold writes on one memo's value, against where the value
    /// is: each swap is a compare-and-swap of the word, so each decision is made on the latest
    /// value, a cold write of a value the memo no longer has is refused by the word's generation,
    /// and every value read (inline copy or row) is read whole. Dropping the store frees each
    /// value's segment once, after every read of it.
    #[test]
    fn model_a_value_row_evicts_refills_and_moves_to_disk_whole() {
        blaze_check::model(|| {
            let (store, memo) = one_memo(b"m0");
            let one = Arc::new(One { store, memo, history: History::new() });
            let evicter = {
                let one = one.clone();
                thread::spawn(move || one.evict())
            };
            let refiller = {
                let one = one.clone();
                thread::spawn(move || {
                    one.refill(1);
                    one.to_disk();
                })
            };
            one.to_disk();
            evicter.join().expect("the evicter");
            refiller.join().expect("the refiller");
            one.look();
            one.history.check(Held::Memory(0));
            copy_follows(&one.store, one.memo);
        });
    }

    /// The finding: an eviction and a refill racing. The refill's swap lands while the
    /// eviction's refresh holds the inline copy; the refill's refresh sees it held and leaves it
    /// to the holder, whose reload of the word must then see the refill. With acquire and
    /// release on those four accesses (store buffering) both could read the old value, and the
    /// copy stayed stale until the next swap; SeqCst on them (`Store::refresh`) forbids it.
    #[test]
    fn model_a_hot_rows_copy_names_its_value_after_racing_swaps() {
        blaze_check::model(|| {
            let (store, memo) = one_memo(b"m0");
            let store = Arc::new(store);
            let evicter = {
                let store = store.clone();
                thread::spawn(move || {
                    let task = blaze_reclaim::enter();
                    store.evict(&task, memo);
                })
            };
            let refiller = {
                let store = store.clone();
                thread::spawn(move || {
                    let task = blaze_reclaim::enter();
                    store.refill(&task, memo, b"m1");
                })
            };
            evicter.join().expect("the evicter");
            refiller.join().expect("the refiller");
            copy_follows(&store, memo);
            let task = blaze_reclaim::enter();
            let bytes = store.bytes(&task, memo);
            assert!(
                bytes.is_none() || bytes == Some(&b"m1"[..]),
                "evicted, or refilled after the eviction: {bytes:?}"
            );
        });
    }

    /// One memo of a shared kind lets go of its row while another thread makes an equal value
    /// and a sweep retires rows no memo names: the maker takes a name on the row (the sweep's
    /// exchange to `DEAD` then fails) or, once it is dead, makes a row of its own and puts it in
    /// the dead one's place; exactly one of the maker and the sweep retires the dead row, and the
    /// maker's value is read whole. Afterwards one row is shared, and none once its memo goes.
    #[test]
    fn model_a_shared_row_is_named_again_or_made_again_while_a_sweep_retires_it() {
        let mut builder = Builder::new();
        // The sweep loads each shard of the shared rows' map: each is a branch.
        builder.max_branches = builder.max_branches.max(8 * blaze_ids::shard_count() + 10_000);
        builder.check(|| {
            const SHARED: Shared = Shared { name: "a model's", schema: SCHEMA };
            let store = Arc::new(Store::new());
            let code = store.code(SCHEMA, ContentId([1; 32]));
            let first = store.make_shared(key(0), Fingerprint(7), code, &[], b"m7", SHARED);
            let maker = {
                let store = store.clone();
                thread::spawn(move || {
                    let memo = store.make_shared(key(1), Fingerprint(7), code, &[], b"m7", SHARED);
                    let task = blaze_reclaim::enter();
                    assert_eq!(store.bytes(&task, memo), Some(&b"m7"[..]), "a shared value read whole");
                    memo
                })
            };
            let leaver = {
                let store = store.clone();
                // Never published: freed at once, letting go of its name.
                thread::spawn(move || store.discard(first))
            };
            // The one thread here in a `blaze_mem::frame::task`.
            store.sweep();
            let made = maker.join().expect("the maker");
            leaver.join().expect("the leaver");
            store.sweep();
            assert_eq!(store.shared_rows(), 1, "the maker's memo names one shared row");
            store.discard(made);
            store.sweep();
            assert_eq!(store.shared_rows(), 0, "no memo names it: retired");
        });
    }

    /// Segments of 64 bytes, so four copies of 16 fill one and the fifth seals it; a copy over
    /// 16 bytes gets a segment of its own, sealed at once. A span lent to another thread is read
    /// whole there while its lane drops its own spans, seals the segment and opens another, and
    /// two threads sweep: a segment is freed only once it is sealed and no span holds it (the
    /// free is checked after every read of its bytes), and then it is freed.
    #[test]
    fn model_a_segment_is_freed_once_sealed_and_let_go_of() {
        blaze_check::model(|| {
            let segments = Arc::new(Segments::new(64, 64, 16));
            let spans: Vec<Span> = (1..=4u8).map(|n| segments.copy(&[n; 16], 16)).collect();
            let lent = spans[0].share();
            let big = segments.copy(&[9; 32], 16);
            let reader = {
                let segments = segments.clone();
                thread::spawn(move || {
                    assert_eq!(lent.bytes(), &[1; 16], "a lent span read whole");
                    assert_eq!(big.bytes(), &[9; 32], "a segment of its own read whole");
                    drop(lent);
                    drop(big);
                    segments.sweep();
                })
            };
            let sweeper = {
                let segments = segments.clone();
                thread::spawn(move || segments.sweep())
            };
            // The open segment stays held by its lane's bias whatever is let go of.
            drop(spans);
            let next = segments.copy(&[5; 16], 16);
            assert_eq!(next.bytes(), &[5; 16]);
            reader.join().expect("the reader");
            sweeper.join().expect("the sweeper");
            segments.sweep();
            assert_eq!(segments.count(), 1, "the sealed segment and the big one are freed; the open one stays");
            drop(next);
            segments.sweep();
            assert_eq!(segments.count(), 1, "an open segment stays while its lane may copy in");
        });
    }
}
