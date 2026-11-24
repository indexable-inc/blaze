//! [`Rows`]: the heads store's tables (`Store::fns`, `adts`, `impls`, ..): keyed rows that are
//! written once, never moved and never dropped before the table, inserted through `&self`.
//!
//! Why (lane VD, 2026-11-17): a body's check borrows its answers from the store for the whole
//! check (`World::fn_sig` hands out `&f.inputs`, `World::adt` the variants, ..). For the check
//! to make a head it meets on the spot instead of ending the round (a round re-runs the body's
//! whole inference), a row filed during the check must not move a row an earlier answer still
//! points into. A `HashMap<K, V>` moves its values when it grows; these tables keep each value
//! in a [`SharedSegs`] (doubling segments, a new one allocated as it grows, none ever moved)
//! and the map holds only the row's index.
//!
//! What makes the rows sound, with no `unsafe` for them here: every value is a published
//! element of the `SharedSegs`, which is written once, never written again through `&self`, and
//! dropped only when the table is (blaze-own segs.rs states that). A reader's `&V` is to that
//! element, never to the index map's buckets, so neither a later insert (which may grow the
//! map), a replacement (a new row; the index moves to it) nor a removal (the key leaves the
//! index; the row stays) can move or free what an answer holds. The index map is behind a
//! `RefCell` borrowed only inside each method, never across a return.
//!
//! Iteration is in the order the live rows were filed: every caller sorts (rule 25), as it
//! did over the map's order.
//!
//! Whose memory (lane RP3, 2026-11-17): the caller's allocator, handed in (`new_in`), for the
//! rows and the index alike, freed when the table drops. Before, each table and each type
//! column reserved its own `blaze_mem` region (an `mmap` at the first row, an `munmap` at the
//! drop), and a heads store (`Store`, `Lives`, `PortableTys`) is made per head and per body:
//! tempo's `engine-check` paid two kernel calls per table per body. Now a store's tables are
//! in the allocator its maker hands `Store::new` (blaze-cli engine_bodies.rs: the request's
//! heap), which maps nothing per table; the rows stay where they were filed because
//! [`SharedSegs`] grows by new segments, never by moving one.

use core::alloc::{Allocator, Layout};
use core::cell::{Cell, RefCell};
use core::hash::Hash;
use core::ptr::NonNull;
use std::alloc::Global;

use blaze_hash::HashMap;
use blaze_own::SharedSegs;

/// The most rows one table holds (rule 18): a table past it stops naming it.
const MOST_ROWS: usize = 1 << 22;

/// Keyed rows, filed through `&self`, each at an address that holds until the table drops; in
/// the allocator `A` the table was made with (the module docs say whose).
pub struct Rows<K, V, A: Allocator + Copy = Global> {
    /// Every row ever filed, with its key, in segments that never move.
    rows: SharedSegs<(K, V), A>,
    /// Each live key's row.
    at: RefCell<HashMap<K, u32, A>>,
}

impl<K: Copy + Eq + Hash, V, A: Allocator + Copy> Rows<K, V, A> {
    /// An empty table in `alloc` (its rows and its index): nothing is allocated until a row is
    /// filed.
    pub fn new_in(alloc: A) -> Rows<K, V, A> {
        Rows { rows: SharedSegs::new_in(alloc), at: RefCell::new(HashMap::new_in(alloc)) }
    }

    fn row(&self, i: u32) -> Option<&(K, V)> {
        self.rows.get(i as usize)
    }

    /// The live row of `k`.
    pub fn get(&self, k: &K) -> Option<&V> {
        let i = *self.at.borrow().get(k)?;
        self.row(i).map(|(_, v)| v)
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.at.borrow().contains_key(k)
    }

    /// Files `v` as `k`'s row: a new row, so every reference to the row it replaces (returned)
    /// still reads what it read.
    pub fn insert(&self, k: K, v: V) -> Option<&V> {
        let filed = self.rows.len();
        if filed >= MOST_ROWS {
            panic!("a heads store's table is past its {MOST_ROWS} rows (front2 rows.rs `MOST_ROWS`)");
        }
        // The one appender: every insert takes it and drops it before returning, and the table
        // is not `Sync`, so another is never out here.
        let Some(mut appender) = self.rows.appender() else {
            panic!("a heads store's table was filed while another insert into it was filing (front2 rows.rs)");
        };
        let i = appender.push((k, v));
        drop(appender);
        // `i` is `filed`, below `MOST_ROWS` (2^22), so it fits.
        let i = i as u32;
        let old = self.at.borrow_mut().insert(k, i)?;
        self.row(old).map(|(_, v)| v)
    }

    /// Files `v` as `k`'s row unless `k` has one already, which stays (a map's
    /// `entry(k).or_insert(v)`); returns the live row.
    pub fn insert_absent(&self, k: K, v: V) -> Option<&V> {
        if !self.contains_key(&k) {
            self.insert(k, v);
        }
        self.get(&k)
    }

    /// Takes `k` out of the index: its row stays where it is (an answer may hold it), and is
    /// returned.
    pub fn remove(&self, k: &K) -> Option<&V> {
        let old = self.at.borrow_mut().remove(k)?;
        self.row(old).map(|(_, v)| v)
    }

    /// Temporarily overrides `k`'s live row with `v`, returning the previous row index so
    /// [`Self::restore_override`] can restore `k` to its original row without copying it.
    pub fn push_override(&self, k: K, v: V) -> Option<u32> {
        let prev = self.at.borrow().get(&k).copied();
        let _ = self.insert(k, v);
        prev
    }

    /// Restores `k`'s live index to `prev` (removing `k` if `prev` is `None`).
    pub fn restore_override(&self, k: K, prev: Option<u32>) {
        match prev {
            Some(i) => {
                let _ = self.at.borrow_mut().insert(k, i);
            }
            None => {
                let _ = self.at.borrow_mut().remove(&k);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.at.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The live rows, in the order filed; rows filed while iterating are not seen.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        let filed = self.rows.len();
        (0..filed).filter_map(move |i| self.rows.get(i).map(|row| (i, row))).filter(move |(i, (k, _))| self.at.borrow().get(k) == Some(&(*i as u32))).map(|(_, (k, v))| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> + '_ {
        self.iter().map(|(_, v)| v)
    }
}

/// `rows[&k]`, as a map's: the caller has checked `k` is filed; a key that is not stops naming it.
impl<K: Copy + Eq + Hash + core::fmt::Debug, V, A: Allocator + Copy> core::ops::Index<&K> for Rows<K, V, A> {
    type Output = V;

    #[track_caller]
    fn index(&self, k: &K) -> &V {
        match self.get(k) {
            Some(v) => v,
            None => panic!("no row is filed for {k:?}"),
        }
    }
}

impl<K: Copy + Eq + Hash + core::fmt::Debug, V: core::fmt::Debug, A: Allocator + Copy> core::fmt::Debug for Rows<K, V, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// The most entries one [`StableColumn`] holds (rule 18): past it an extend is `Full`.
const MOST_ENTRIES: usize = 1 << 26;

/// The room of a column's first buffer, in entries.
const FIRST_ROOM: usize = 64;

/// The most buffers one column is ever grown through: each growth at least doubles the room
/// from [`FIRST_ROOM`] (2^6) to at most [`MOST_ENTRIES`] (2^26), so 21 are ever used.
const BUFFERS: usize = 32;

/// A type table's column (blaze-typeck `Stable`, the `&self` form of `PortableTys`), in the
/// allocator it was made with: contiguous (the type table reads it as one slice), and nothing it
/// holds moves while a view of it is held.
///
/// How, through `&self`: a growth copies the entries into a buffer twice as large and KEEPS
/// the old one until the column drops, so a slice handed out before (`as_slice`, a view's
/// snapshot) still reads what it read, in memory that is never written again. The cost: the
/// old buffers, under the live one's room in all (a geometric sum), and one copy of each entry
/// per doubling; a column that interns nothing allocates nothing. Past [`MOST_ENTRIES`], or
/// memory that cannot be had, is `Full`.
pub struct StableColumn<T: Copy, A: Allocator> {
    /// Each buffer ever grown to, oldest first, with its room: the last holds every entry,
    /// each older one a prefix of them. `count` of them are set.
    bufs: RefCell<[(*mut T, usize); BUFFERS]>,
    count: Cell<usize>,
    /// Entries written: the first `len` of the last buffer.
    len: Cell<usize>,
    alloc: A,
}

impl<T: Copy, A: Allocator> StableColumn<T, A> {
    /// An empty column in `alloc`: nothing is allocated until an entry is.
    pub fn new_in(alloc: A) -> StableColumn<T, A> {
        StableColumn { bufs: RefCell::new([(core::ptr::null_mut(), 0); BUFFERS]), count: Cell::new(0), len: Cell::new(0), alloc }
    }

    /// The live buffer and its room; `(null, 0)` before the first entry.
    fn live(&self) -> (*mut T, usize) {
        match self.count.get() {
            0 => (core::ptr::null_mut(), 0),
            n => self.bufs.borrow()[n - 1],
        }
    }
}

impl<T: Copy, A: Allocator> blaze_typeck::Stable<T> for StableColumn<T, A> {
    fn extend(&self, values: &[T]) -> Result<u32, blaze_typeck::Full> {
        let len = self.len.get();
        let first = u32::try_from(len).map_err(|_| blaze_typeck::Full)?;
        let end = len.checked_add(values.len()).filter(|&end| end <= MOST_ENTRIES).ok_or(blaze_typeck::Full)?;
        if values.is_empty() {
            return Ok(first);
        }
        let (mut at, room) = self.live();
        if end > room {
            let count = self.count.get();
            if count == BUFFERS {
                return Err(blaze_typeck::Full);
            }
            let grown = room.saturating_mul(2).max(end).max(FIRST_ROOM).min(MOST_ENTRIES);
            let layout = Layout::array::<T>(grown).map_err(|_| blaze_typeck::Full)?;
            let made = self.alloc.allocate(layout).map_err(|_| blaze_typeck::Full)?.cast::<T>().as_ptr();
            if len > 0 {
                // SAFETY: `at` is the live buffer, whose first `len` entries are written
                // (below, on earlier extends); `made` is a fresh allocation of `grown >= end >
                // len` entries, so the two do not overlap and both are aligned for `T`.
                unsafe { core::ptr::copy_nonoverlapping(at, made, len) };
            }
            self.bufs.borrow_mut()[count] = (made, grown);
            self.count.set(count + 1);
            at = made;
        }
        // SAFETY: the live buffer `at` has room for `end` entries; slots `len..end` are past
        // every slice handed out of it (each ends at the `len` of its time, at most this one)
        // and past every buffer kept, so no reader sees them until `len` is set below; `values`
        // is a live borrow, so it is no unwritten slot of this buffer and does not overlap them.
        unsafe { core::ptr::copy_nonoverlapping(values.as_ptr(), at.add(len), values.len()) };
        self.len.set(end);
        Ok(first)
    }

    fn as_slice(&self) -> &[T] {
        let (at, _) = self.live();
        if at.is_null() {
            return &[];
        }
        // SAFETY: the first `len` entries of the live buffer are written and never written
        // again (a growth copies them into a new buffer and leaves these as they are), and a
        // buffer is freed only when the column drops (`&mut self`), so they outlive `&self`.
        unsafe { core::slice::from_raw_parts(at, self.len.get()) }
    }
}

impl<T: Copy, A: Allocator> Drop for StableColumn<T, A> {
    fn drop(&mut self) {
        let count = self.count.get();
        for &(at, room) in &self.bufs.get_mut()[..count] {
            let (Some(at), Ok(layout)) = (NonNull::new(at), Layout::array::<T>(room)) else { continue };
            // SAFETY: allocated by `self.alloc` in `extend` with exactly this layout (`room`
            // entries of `T`), freed once here; `&mut self`: no slice of it is left. The
            // entries are `Copy`, so nothing is dropped.
            unsafe { self.alloc.deallocate(at.cast::<u8>(), layout) };
        }
    }
}

/// A type table over [`StableColumn`]s in `alloc` (blaze-typeck `PortableTys::new_stable`):
/// interned through `&self` too, its columns in `alloc` (the module docs say whose).
pub fn stable_tys(alloc: blaze_own::Alloc<'_>) -> Result<blaze_typeck::PortableTys<'_>, blaze_typeck::CheckError> {
    let nodes: blaze_own::Box<dyn blaze_typeck::Stable<blaze_typeck::PNode>, blaze_own::Alloc<'_>> = blaze_own::Box::new_in(StableColumn::new_in(alloc), alloc);
    let lists: blaze_own::Box<dyn blaze_typeck::Stable<blaze_typeck::PTy>, blaze_own::Alloc<'_>> = blaze_own::Box::new_in(StableColumn::new_in(alloc), alloc);
    let record_schemas: blaze_own::Box<dyn blaze_typeck::Stable<(blaze_typeck::RecordSchema, blaze_typeck::List)>, blaze_own::Alloc<'_>> =
        blaze_own::Box::new_in(StableColumn::new_in(alloc), alloc);
    let record_names: blaze_own::Box<dyn blaze_typeck::Stable<blaze_typeck::Name>, blaze_own::Alloc<'_>> =
        blaze_own::Box::new_in(StableColumn::new_in(alloc), alloc);
    blaze_typeck::PortableTys::new_stable(alloc, nodes, lists, record_schemas, record_names)
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    /// A type table over stable columns interns as one over owned columns does (equal ids,
    /// equal tables), through `&self`, while a view taken first still reads what it read; the
    /// owned form refuses a `&self` intern by type of error. The control: the owned form's
    /// `Vec` moves its nodes when it grows.
    #[test]
    fn a_stable_type_table_interns_alike_through_a_shared_borrow() {
        use blaze_typeck::{CheckError, PNode, PTy, PortableTys};
        let g = blaze_own::Alloc::global();
        let stable = stable_tys(g).expect("the stable table");
        let mut owned = PortableTys::new_in(g);
        let unit = PNode::Tuple(blaze_typeck::List::EMPTY);
        let first = stable.intern_shared(unit).expect("interned");
        assert_eq!(owned.intern(unit).expect("interned"), first);
        let held = stable.view();
        let held_len = held.len();
        let mut last = first;
        for i in 0..50_000u32 {
            let list = stable.intern_list_shared(&[last, first]).expect("a list");
            assert_eq!(owned.intern_list(&[last, first]).expect("a list"), list);
            let n = PNode::Tuple(list);
            last = stable.intern_shared(n).expect("interned");
            assert_eq!(owned.intern(n).expect("interned"), last);
            assert_eq!(stable.intern_shared(n).expect("again"), last, "hash-consed at {i}");
        }
        assert!(stable == owned, "equal tables");
        // The view taken before 100,000 interns: its length and node as they were.
        assert_eq!(held.len(), held_len);
        assert_eq!(held.node(first).ok(), Some(unit));
        assert_eq!(stable.view().len(), 50_001);
        assert!(matches!(owned.intern_shared(unit), Err(CheckError::Malformed(blaze_typeck::Malformed::SharedIntern))));
        let _: PTy = last;
    }

    /// A LIVE view (blaze-typeck `PTys`, lane VH): a view of a stable table taken, then 1,000
    /// types and their lists interned through `&self` (what an import during a body's check
    /// does): (a) each is read through the OLD view, equal to a fresh view, and through a
    /// seeded body's view (`OwnTys::in_base`) taken before them; (b) their ids are dense and
    /// unique; (c) the owned form reads the same nodes and lists through the same accessors.
    /// The control (d): a view of the same snapshot WITHOUT the column (`PTys::flat`) refuses a
    /// late id by type of error (`Malformed::TypeOrder`; a late list: `Malformed::List`).
    #[test]
    fn a_view_taken_before_an_import_reads_it() {
        use blaze_typeck::{CheckError, List, Malformed, OwnTys, PNode, PTy, PTys, PortableTys};
        let g = blaze_own::Alloc::global();
        let stable = stable_tys(g).expect("the stable table");
        let mut owned = PortableTys::new_in(g);
        let unit = PNode::Tuple(List::EMPTY);
        let first = stable.intern_shared(unit).expect("interned");
        assert_eq!(owned.intern(unit).expect("interned"), first);
        let old = stable.view();
        let old_len = old.len();
        let seeded = OwnTys::in_base(g);
        let body_view = seeded.view(&stable).expect("a seeded body's view");
        let mut made: Vec<(PTy, List), Global> = Vec::new_in(Global);
        let mut last = first;
        for _ in 0..1_000u32 {
            let list = stable.intern_list_shared(&[last, first]).expect("a list");
            let t = stable.intern_shared(PNode::Tuple(list)).expect("interned");
            assert_eq!(owned.intern_list(&[last, first]).expect("a list"), list);
            assert_eq!(owned.intern(PNode::Tuple(list)).expect("interned"), t);
            made.push((t, list));
            last = t;
        }
        // (b) Dense and unique: the k-th type made is `PTy(k + 1)`, after `()`.
        for (k, &(t, _)) in made.iter().enumerate() {
            assert_eq!(t.0 as usize, k + 1, "ids dense and unique");
        }
        let fresh = stable.view();
        let owned_view = owned.view();
        assert_eq!(old.len(), old_len, "the old view's snapshot is as it was");
        assert_eq!(old.len_now(), fresh.len(), "the old view's column as it stands");
        for &(t, list) in made.iter() {
            assert_eq!(old.node(t).ok(), Some(PNode::Tuple(list)), "(a) read through the old view");
            assert_eq!(old.node(t).ok(), fresh.node(t).ok());
            assert_eq!(body_view.node(t).ok(), fresh.node(t).ok(), "(a) through the seeded body's view");
            assert_eq!(old.list(list).ok(), fresh.list(list).ok());
            assert_eq!(body_view.list(list).ok(), fresh.list(list).ok());
            for i in list.range() {
                assert_eq!(old.list_slot(i), fresh.list_slot(i));
            }
            // (c) The owned form, through the same accessors.
            assert_eq!(owned_view.node(t).ok(), Some(PNode::Tuple(list)));
            assert_eq!(owned_view.list(list).ok(), fresh.list(list).ok());
        }
        // (d) The control: the old snapshot without the column.
        let mut nodes = Vec::new_in(Global);
        nodes.extend(old.nodes().copied());
        let mut lists = Vec::new_in(Global);
        lists.extend(old.lists().copied());
        let snapshot = PTys::flat(&nodes, &lists);
        assert_eq!(snapshot.node(first).ok(), Some(unit));
        assert!(matches!(snapshot.node(made[0].0), Err(CheckError::Malformed(Malformed::TypeOrder { .. }))), "a late id past a snapshot is refused");
        assert!(matches!(snapshot.list(made[0].1), Err(CheckError::Malformed(Malformed::List { .. }))), "a late list past a snapshot is refused");
    }

    /// A row read before many more are filed (past the first commit, and past the index map's
    /// growth), replaced and removed, is still at its address with its value: the property the
    /// on-the-spot head rests on. The control: the same through a `Vec` moves the row.
    #[test]
    fn a_row_never_moves() {
        let rows: Rows<u64, Vec<u64, Global>> = Rows::new_in(Global);
        let mut first = Vec::new_in(Global);
        first.extend_from_slice(&[7, 8, 9]);
        rows.insert(1, first);
        let held: &Vec<u64, Global> = rows.get(&1).expect("filed");
        let at = held as *const Vec<u64, Global> as usize;
        let data = held.as_ptr() as usize;
        for k in 2..100_000u64 {
            let mut v = Vec::new_in(Global);
            v.push(k);
            rows.insert(k, v);
        }
        let mut again = Vec::new_in(Global);
        again.push(1);
        let replaced = rows.insert(1, again).expect("the old row");
        assert_eq!(replaced as *const _ as usize, at);
        assert_eq!(rows.get(&1).map(|v| &v[..]), Some(&[1u64][..]));
        assert!(rows.remove(&5).is_some());
        assert!(!rows.contains_key(&5));
        // The reference taken first reads its row unchanged, at the same address.
        assert_eq!(held as *const _ as usize, at);
        assert_eq!(held.as_ptr() as usize, data);
        assert_eq!(&held[..], &[7, 8, 9]);
        assert_eq!(rows.len(), 99_998);
        // Live rows only, in the order filed: the replaced row of 1 and the removed 5 are not.
        let mut keys = Vec::new_in(Global);
        keys.extend(rows.keys().copied().take(3));
        assert_eq!(&keys[..], &[2, 3, 4]);
        assert_eq!(rows.iter().count(), 99_998);
        // Control: a `Vec`'s element moves when it grows.
        let mut v: Vec<u64, Global> = Vec::with_capacity_in(1, Global);
        v.push(0);
        let before = v.as_ptr() as usize;
        v.extend(1..100_000u64);
        assert_ne!(v.as_ptr() as usize, before);
    }
}
